import Foundation
import XCTest
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration

private actor ConnectionPromptOwner: WorkspaceSSHPasswordPrompting, WorkspaceSSHKeyboardInteractivePrompting {
    private(set) var passwordTransports: [UUID] = []
    private(set) var interactiveTransports: [UUID] = []

    func password(for request: WorkspaceSSHPasswordRequest) throws -> Data {
        passwordTransports.append(request.transportID)
        return Data("files owner password".utf8)
    }

    func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) throws -> [Data] {
        guard request.challenge.round == 1, request.challenge.name == "Files ownership",
              request.challenge.prompts.count == 1,
              request.challenge.prompts[0].text == "Approval:",
              !request.challenge.prompts[0].echo else { throw SSHAuthenticationError.invalidChallenge }
        interactiveTransports.append(request.transportID)
        return [Data("approved".utf8)]
    }

    nonisolated func cancel(requestID: UUID) {}
}

@MainActor
final class WorkspaceAuthenticationPrompterTests: XCTestCase {
    private func checkMissingDelegate(_ choice: WorkspaceSSHAuthenticationChoice) async throws {
        let fixture = try await SSHWorkspaceTestFixture.make { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = ["publickey", "password", "keyboard-interactive"]
            workspace["routes"] = routes
        }
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("prompt-owners-" + UUID().uuidString)
        let journal = try TerminalSessionJournal(directory: root)
        let activity = WorkspaceNetworkActivity(), defaults = ConnectionPromptOwner(), selected = ConnectionPromptOwner()
        let manager = WorkspaceSSHConnectionManager(store: fixture.store, broker: fixture.broker,
            journal: journal, coordinator: try TerminalInputCoordinator(), networkActivity: activity,
            passwordPrompter: defaults, keyboardInteractivePrompter: defaults)
        await manager.resume()
        let prompts = choice == .password
            ? WorkspaceSSHAuthenticationPrompters(keyboardInteractive: selected)
            : WorkspaceSSHAuthenticationPrompters(password: selected)
        let selection = WorkspaceSSHConnectionSelection(workspaceID: fixture.installed.id,
            expectedRevision: fixture.installed.revision, machineID: "machine", routeID: "route", authentication: choice)
        do {
            _ = try await manager.connect(workspaceID: fixture.installed.id,
                expectedRevision: fixture.installed.revision, machineID: "machine", routeID: "route",
                authentication: choice, confirmedByUser: true, connectionTimeout: .milliseconds(20),
                selection: selection, authenticationPrompters: prompts)
            XCTFail("An explicitly absent delegate must not borrow the manager's default editor")
        } catch {
            XCTAssertEqual(error as? SSHAuthenticationError, .unavailable)
        }
        let defaultPassword = await defaults.passwordTransports, defaultInteractive = await defaults.interactiveTransports
        let selectedPassword = await selected.passwordTransports, selectedInteractive = await selected.interactiveTransports
        XCTAssertTrue(defaultPassword.isEmpty); XCTAssertTrue(defaultInteractive.isEmpty)
        XCTAssertTrue(selectedPassword.isEmpty); XCTAssertTrue(selectedInteractive.isEmpty)
        let pending = await manager.pendingCount, connections = await manager.connections()
        XCTAssertEqual(pending, 0); XCTAssertTrue(connections.isEmpty)
        XCTAssertEqual(activity.accounting().connections, 0)
        XCTAssertEqual(fixture.vault.counts.reads, 0); XCTAssertEqual(fixture.vault.counts.signatures, 0)
        await manager.suspend()
    }

    func testExplicitMissingPasswordDelegateNeverBorrowsManagerDefault() async throws {
        try await checkMissingDelegate(.password)
    }

    func testExplicitMissingInteractiveDelegateNeverBorrowsManagerDefault() async throws {
        try await checkMissingDelegate(.keyboardInteractive)
    }

    /// The existing SFTP runner supplies a real server that accepts two
    /// sequential logins on the exact same route. The first attempt uses its
    /// reviewed Files owner; the second must return to the manager defaults.
    func testLivePromptDelegatesRemainBoundToOneFileConnectionAttempt() async throws {
        struct Peer: Decodable { let id: String; let port: UInt16; let hostKey: String }
        let environment = ProcessInfo.processInfo.environment
        guard let mode = environment["FT_SFTP_TEST_MODE"], mode.hasPrefix("scope-"),
              let rootPath = environment["FT_SFTP_TEST_ROOT"],
              let bytes = environment["FT_SFTP_TEST_PEERS"]?.data(using: .utf8) else {
            throw XCTSkip("Run Tests/Interop/test_sftp.py with the prompt-owner scenarios")
        }
        XCTAssertTrue(NativeSSHSession.supportsSFTP)
        let peer = try XCTUnwrap(JSONDecoder().decode([Peer].self, from: bytes).first { $0.id == "target" })
        let fixture = try await SSHWorkspaceTestFixture.make(hostname: "127.0.0.1", port: peer.port,
            hostBlob: XCTUnwrap(Data(base64Encoded: peer.hostKey))) { workspace in
                var routes = workspace["routes"] as! [[String: Any]]
                routes[0]["authentication_methods"] = ["publickey", "password", "keyboard-interactive"]
                workspace["routes"] = routes
            }
        let journal = try TerminalSessionJournal(directory: URL(fileURLWithPath: rootPath).appendingPathComponent("journal"))
        let activity = WorkspaceNetworkActivity(), defaults = ConnectionPromptOwner(), filesOwner = ConnectionPromptOwner()
        let manager = WorkspaceSSHConnectionManager(store: fixture.store, broker: fixture.broker,
            journal: journal, coordinator: try TerminalInputCoordinator(), networkActivity: activity,
            passwordPrompter: defaults, keyboardInteractivePrompter: defaults)
        await manager.resume()
        var transports: [UUID] = []
        do {
            for index in 0..<2 {
                let prompts = index == 0
                    ? WorkspaceSSHAuthenticationPrompters(password: filesOwner, keyboardInteractive: filesOwner) : nil
                let connection: WorkspaceSSHConnection
                if mode == "scope-publickey-interactive" {
                    // Cover the convenience overload and its MFA continuation.
                    connection = try await manager.connect(workspaceID: fixture.installed.id,
                        expectedRevision: fixture.installed.revision, machineID: "machine", routeID: "route", credentialID: "key",
                        confirmedByUser: true, connectionTimeout: .seconds(3), authenticationTimeout: .seconds(3),
                        attachMux: false, purpose: .fileTransfer, authenticationPrompters: prompts)
                } else {
                    let authentication: WorkspaceSSHAuthenticationChoice = mode == "scope-password" ? .password : .keyboardInteractive
                    let selection = WorkspaceSSHConnectionSelection(workspaceID: fixture.installed.id,
                        expectedRevision: fixture.installed.revision, machineID: "machine", routeID: "route",
                        authentication: authentication)
                    connection = try await manager.connect(workspaceID: fixture.installed.id,
                        expectedRevision: fixture.installed.revision, machineID: "machine", routeID: "route", authentication: authentication,
                        confirmedByUser: true, connectionTimeout: .seconds(3), authenticationTimeout: .seconds(3),
                        attachMux: false, selection: selection, purpose: .fileTransfer, authenticationPrompters: prompts)
                }
                transports.append(connection.scope.transportID)
                let files = try await connection.openFiles()
                let listing = try await files.listDirectory(path: ".")
                XCTAssertTrue(listing.entries.contains { $0.name == "download-source.bin" })
                await files.close()
                try await manager.close(connectionID: connection.id)
                let defaultCount = mode == "scope-password" ? await defaults.passwordTransports.count : await defaults.interactiveTransports.count
                let ownerCount = mode == "scope-password" ? await filesOwner.passwordTransports.count : await filesOwner.interactiveTransports.count
                XCTAssertEqual(defaultCount, index)
                XCTAssertEqual(ownerCount, 1)
            }
            XCTAssertEqual(Set(transports).count, 2)
            let defaultTransports = mode == "scope-password" ? await defaults.passwordTransports : await defaults.interactiveTransports
            let ownerTransports = mode == "scope-password" ? await filesOwner.passwordTransports : await filesOwner.interactiveTransports
            XCTAssertEqual(ownerTransports, [transports[0]])
            XCTAssertEqual(defaultTransports, [transports[1]])
            let retained = await manager.connections(), records = try await journal.records()
            XCTAssertTrue(retained.isEmpty); XCTAssertTrue(records.isEmpty)
            XCTAssertEqual(activity.accounting().connections, 0)
        } catch {
            await manager.suspend()
            throw error
        }
        await manager.suspend()
    }
}
