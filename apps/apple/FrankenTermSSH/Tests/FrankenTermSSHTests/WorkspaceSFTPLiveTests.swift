import Foundation
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
@testable import FrankenTermSSHIntegration

private actor SFTPProgressProbe {
    private var values: [(UInt64, UInt64?)] = []
    func append(_ completed: UInt64, _ total: UInt64?) { values.append((completed, total)) }
    func snapshot() -> [(UInt64, UInt64?)] { values }
}

/// The companion Python runner supplies independent libssh SSH peers connected
/// to the real OpenSSH SFTP server. Ordinary package tests skip this fixture;
/// the runner requires one executed, unskipped test for every selected scenario.
@MainActor
final class WorkspaceSFTPLiveTests: XCTestCase {
    private struct Peer: Decodable {
        let id: String
        let port: UInt16
        let hostKey: String
    }

    private func waitForMarker(_ url: URL) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while !FileManager.default.fileExists(atPath: url.path) {
            guard ContinuousClock.now < deadline else {
                throw NSError(domain: "SFTP fixture marker was not observed", code: 1)
            }
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    func testLiveSFTPTransfersStayBoundToWorkspaceAuthority() async throws {
        let environment = ProcessInfo.processInfo.environment
        guard let mode = environment["FT_SFTP_TEST_MODE"],
              let rootPath = environment["FT_SFTP_TEST_ROOT"],
              let encodedPeers = environment["FT_SFTP_TEST_PEERS"]?.data(using: .utf8) else {
            throw XCTSkip("Run Tests/Interop/test_sftp.py with independent loopback peers")
        }
        XCTAssertTrue(NativeSSHSession.supportsSFTP)
        let root = URL(fileURLWithPath: rootPath, isDirectory: true)
        let peers = try JSONDecoder().decode([Peer].self, from: encodedPeers)
        let target = try XCTUnwrap(peers.first { $0.id == "target" })
        let targetHost = try XCTUnwrap(Data(base64Encoded: target.hostKey))
        let identities = try Dictionary(uniqueKeysWithValues: peers.map {
            ($0.id, try SSHPublicIdentity(blob: XCTUnwrap(Data(base64Encoded: $0.hostKey))))
        })
        let fixture = try await SSHWorkspaceTestFixture.make(
            hostname: mode == "jump" ? "target.internal.invalid" : "127.0.0.1",
            port: target.port, hostBlob: targetHost
        ) { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            var trusts = workspace["host_trust"] as! [[String: Any]]
            let template = routes[0]
            for peer in peers where peer.id != "target" {
                var route = template
                route["id"] = peer.id
                route["host"] = "127.0.0.1"
                route["port"] = Int(peer.port)
                route["host_trust_ref"] = "trust-\(peer.id)"
                route["jump_routes"] = [String]()
                if peer.id == "hop" {
                    route["mode"] = "frankenterm_mux"
                    route["session_creation"] = nil
                    route["attach_policy"] = "existing_only"
                    routes[0]["jump_routes"] = ["hop"]
                }
                routes.append(route)
                let identity = identities[peer.id]!
                trusts.append(["id": "trust-\(peer.id)", "host": "127.0.0.1", "port": Int(peer.port),
                    "revoked_keys": [], "pins": [["algorithm": identity.algorithm, "fingerprint": identity.fingerprint]]])
            }
            if mode == "ownership" {
                var machines = workspace["machines"] as! [[String: Any]]
                machines[0]["route_refs"] = ["route", "terminal"]
                for routeID in ["second", "third"] {
                    machines.append(["id": "machine-\(routeID)", "name": routeID,
                        "theme_ref": "theme", "route_refs": [routeID],
                        "preferred_route_ref": routeID, "auto_connect": false])
                }
                var groups = workspace["groups"] as! [[String: Any]]
                groups[0]["machine_order"] = ["machine", "machine-second", "machine-third"]
                workspace["groups"] = groups
                workspace["machines"] = machines
            }
            workspace["routes"] = routes
            workspace["host_trust"] = trusts
        }

        if mode == "timeout-stall" || mode == "cancel-init" {
            try await exerciseNativeRetirement(fixture, mode: mode, root: root)
            return
        }

        let activity = WorkspaceNetworkActivity()
        let journal = try TerminalSessionJournal(directory: root.appendingPathComponent("journal", isDirectory: true))
        let coordinator = try TerminalInputCoordinator()
        let manager = WorkspaceSSHConnectionManager(store: fixture.store, broker: fixture.broker,
            journal: journal, coordinator: coordinator, networkActivity: activity)
        await manager.resume()

        func connect(_ machine: String = "machine", route: String = "route",
                     purpose: WorkspaceSSHConnectionPurpose = .fileTransfer) async throws -> WorkspaceSSHConnection {
            try await manager.connect(workspaceID: fixture.installed.id,
                expectedRevision: fixture.installed.revision, machineID: machine, routeID: route,
                authentication: .publicKey("key"), confirmedByUser: true,
                connectionTimeout: .seconds(3), authenticationTimeout: .seconds(3),
                attachMux: false, purpose: purpose)
        }

        do {
            let connection = try await connect()
            XCTAssertEqual(connection.purpose, .fileTransfer)
            let files = try await connection.openFiles()
            switch mode {
            case "direct", "jump":
                let listing = try await files.listDirectory(path: ".")
                XCTAssertTrue(listing.path.hasPrefix("/"))
                XCTAssertFalse(listing.isTruncated)
                let source = try XCTUnwrap(listing.entries.first { $0.name == "download-source.bin" })
                XCTAssertEqual(source.attributes.kind, .regularFile)
                XCTAssertEqual(source.attributes.size, 131_113)
                XCTAssertTrue(listing.entries.contains { $0.name == "résumé λ.bin" })
                XCTAssertEqual(listing.entries.first { $0.name == "directory" }?.attributes.kind, .directory)
                let limited = try await files.listDirectory(path: listing.path, limit: 1)
                XCTAssertEqual(limited.entries.count, 1)
                XCTAssertTrue(limited.isTruncated)
                let remote = URL(fileURLWithPath: listing.path, isDirectory: true)
                let progress = SFTPProgressProbe()
                let local = root.appendingPathComponent("local/downloaded.bin")
                let download = try await files.download(path: remote.appendingPathComponent("download-source.bin").path,
                    to: local, onProgress: { await progress.append($0, $1) })
                XCTAssertEqual(download.direction, .download)
                XCTAssertEqual(download.transferredBytes, 131_113)
                XCTAssertEqual(try Data(contentsOf: local), Data((0..<131_113).map { UInt8(truncatingIfNeeded: $0 * 37 + 11) }))
                let reported = await progress.snapshot()
                XCTAssertEqual(reported.first?.0, 0)
                XCTAssertEqual(reported.last?.0, 131_113)
                XCTAssertTrue(reported.allSatisfy { $0.1 == 131_113 })
                XCTAssertTrue(zip(reported, reported.dropFirst()).allSatisfy { $0.0.0 <= $0.1.0 })
                let upload = try await files.upload(from: root.appendingPathComponent("local/upload-source.bin"),
                    to: remote.appendingPathComponent("uploaded.bin").path)
                XCTAssertEqual(upload.direction, .upload)
                XCTAssertEqual(upload.transferredBytes, 98_327)
                do {
                    _ = try await files.upload(from: root.appendingPathComponent("local/upload-source.bin"),
                        to: remote.appendingPathComponent("existing.bin").path)
                    XCTFail("An exclusive upload must not replace an existing remote file")
                } catch let error as WorkspaceFileTransferFailure {
                    XCTAssertEqual(error.transferredBytes, 0)
                    XCTAssertFalse(error.localizedDescription.contains("existing.bin"))
                }
            case "cancel-stall", "cancel-upload":
                let upload = mode == "cancel-upload"
                let operation = Task {
                    if upload {
                        return try await files.upload(from: root.appendingPathComponent("local/upload-source.bin"),
                            to: root.appendingPathComponent("cancelled-upload.bin").path)
                    }
                    return try await files.download(path: root.appendingPathComponent("download-source.bin").path,
                        to: root.appendingPathComponent("local/cancelled-download.bin"))
                }
                do { try await waitForMarker(root.appendingPathComponent(upload ? "write-seen" : "read-seen")) }
                catch { operation.cancel(); _ = await operation.result; throw error }
                operation.cancel()
                do { _ = try await operation.value; XCTFail("A cancelled transfer must not return a receipt") }
                catch let error as WorkspaceFileTransferFailure {
                    if case .cancelled = error.reason {} else { XCTFail("Expected transfer cancellation") }
                    XCTAssertEqual(error.transferredBytes, 0)
                    XCTAssertEqual(error.remoteFileMayRemain, upload)
                    XCTAssertFalse(error.localizedDescription.contains(root.path))
                }
                let closed = await connection.isClosed
                XCTAssertTrue(closed)
                XCTAssertEqual(activity.accounting().connections, 0)
            case "revoke", "stale":
                _ = try await files.listDirectory(path: ".")
                if mode == "revoke" {
                    try await fixture.store.revokeHostLocally(workspaceID: fixture.installed.id,
                        trustID: "trust", key: fixture.host)
                } else {
                    var manifest = try XCTUnwrap(JSONSerialization.jsonObject(with: fixture.installed.manifest) as? [String: Any])
                    manifest["bundle_id"] = "sftp-reviewed-replacement"
                    var document = manifest["workspace"] as! [String: Any]
                    document["name"] = "Reviewed replacement"
                    manifest["workspace"] = document
                    let replacement = try AirGapArchive(workspace: WorkspaceDocument(data:
                        JSONSerialization.data(withJSONObject: manifest, options: [.sortedKeys])),
                        keys: fixture.archive.keys, using: SSHWorkspaceTestCryptography())
                    let installed = try await fixture.store.install(replacement, envelopeDigest: Data(repeating: 2, count: 32),
                        expectedRevision: fixture.installed.revision, sourceAndContentsConfirmed: true,
                        associateWithExistingWorkspace: true).workspace
                    XCTAssertNotEqual(installed.revision, fixture.installed.revision)
                }
                try Data("retired\n".utf8).write(to: root.appendingPathComponent("retired"), options: .withoutOverwriting)
                do { _ = try await files.listDirectory(path: "."); XCTFail("Retired authority must not issue another operation") }
                catch { XCTAssertFalse(error.localizedDescription.contains(root.path)) }
                let closed = await connection.isClosed
                XCTAssertTrue(closed)
                XCTAssertEqual(activity.accounting().connections, 0)
            case "ownership":
                let terminal = try await connect(route: "terminal", purpose: .terminal)
                XCTAssertEqual(terminal.purpose, .terminal)
                let second = try await connect("machine-second", route: "second")
                let secondFiles = try await second.openFiles()
                _ = try await files.listDirectory(path: ".")
                _ = try await secondFiles.listDirectory(path: ".")
                XCTAssertEqual(activity.accounting().connections, 3)
                do { _ = try await connect("machine-third", route: "third"); XCTFail("The third file connection must exceed its budget") }
                catch { XCTAssertEqual(error as? WorkspaceSSHConnectionError, .capacity) }
                try await manager.close(connectionID: connection.id)
                XCTAssertEqual(activity.accounting().connections, 2)
                let terminalClosedByFile = await terminal.isClosed
                XCTAssertFalse(terminalClosedByFile)
                _ = try await secondFiles.listDirectory(path: ".")
                let third = try await connect("machine-third", route: "third")
                let thirdFiles = try await third.openFiles()
                _ = try await thirdFiles.listDirectory(path: ".")
                XCTAssertEqual(activity.accounting().connections, 3)
                await manager.invalidate(workspaceID: fixture.installed.id)
                XCTAssertEqual(activity.accounting().connections, 0)
                let terminalClosed = await terminal.isClosed
                let secondClosed = await second.isClosed
                let thirdClosed = await third.isClosed
                XCTAssertTrue(terminalClosed)
                XCTAssertTrue(secondClosed)
                XCTAssertTrue(thirdClosed)
            default:
                XCTFail("Unknown SFTP fixture scenario")
            }
            try await manager.close(connectionID: connection.id)
            await manager.suspend()
            XCTAssertEqual(activity.accounting().connections, 0)
            let records = try await journal.records()
            XCTAssertTrue(records.isEmpty, "File connections must not create terminal session epochs")
        } catch {
            await manager.suspend()
            throw error
        }
    }

    private func exerciseNativeRetirement(_ fixture: SSHWorkspaceTestFixture, mode: String, root: URL) async throws {
        let session = try await NativeSSHConnector.connect(to: fixture.endpoint, timeout: .seconds(3))
        do {
            _ = try await session.authenticate(using: fixture.authority(transport: session.transportID), timeout: .seconds(3))
            if mode == "cancel-init" {
                let operation = Task { try await session.startSFTP(timeout: .seconds(3)) }
                do { try await waitForMarker(root.appendingPathComponent("init-seen")) }
                catch { operation.cancel(); _ = await operation.result; throw error }
                try await Task.sleep(for: .milliseconds(20))
                operation.cancel()
                do { try await operation.value; XCTFail("A cancelled SFTP initialization must not succeed") }
                catch is CancellationError {}
            } else {
                try await session.startSFTP(timeout: .seconds(3))
                _ = try await session.openSFTPFile(path: "download-source.bin", mode: .read, timeout: .seconds(3))
                let started = ContinuousClock.now
                do {
                    _ = try await session.readSFTPFile(maximumBytes: 65_536, timeout: .milliseconds(150))
                    XCTFail("A stalled SFTP read must reach its deadline")
                } catch { XCTAssertEqual(error as? NativeSSHError, .timedOut) }
                XCTAssertLessThan(started.duration(to: .now), .seconds(2))
            }
            await session.close()
            let accounting = try await session.accounting()
            XCTAssertEqual(accounting.allocatedBytes, 0)
            XCTAssertEqual(accounting.pendingWriteBytes, 0)
            XCTAssertEqual(NativeSSHConnector.accounting().connectingSockets, 0)
        } catch {
            await session.close()
            throw error
        }
    }
}
