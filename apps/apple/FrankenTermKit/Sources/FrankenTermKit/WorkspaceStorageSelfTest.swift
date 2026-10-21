#if os(iOS)
import Foundation
import CryptoKit
import LocalAuthentication

/// Runs on the device exactly what a restore's save does, the protected
/// Keychain and the workspace store, with a throwaway key in a throwaway
/// store, and reports every step: remote diagnosis of a save that fails only
/// on hardware. The app runs it when launched with FT_STORAGE_SELF_TEST=1 and
/// removes the key it stored.
public enum WorkspaceStorageSelfTest {
    public static func run(directory: URL) async -> [String] {
        var report: [String] = []
        let crypto = AppleCryptography(), vault = AppleWorkspaceVault()
        let context = LAContext()
        var policyError: NSError?
        let passcode = context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &policyError)
        report.append("passcode set: \(passcode)\(policyError.map { " (\($0.domain) \($0.code))" } ?? "")")
        do {
            let (key, identity) = try throwawayKey(using: crypto)
            let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: workspace(identity),
                                                                              options: [.sortedKeys]))
            let archive = try AirGapArchive(workspace: document, keys: [key], using: crypto)
            report.append("ok: throwaway key and archive")
            let store = WorkspaceStore(persistence: try FileWorkspacePersistence(directory: directory), vault: vault)
            let receipt = try await store.install(archive, envelopeDigest: crypto.sha256(Data(UUID().uuidString.utf8)),
                                                  transferID: Data((0..<16).map { _ in UInt8.random(in: 0...255) }),
                                                  expectedRevision: nil, sourceAndContentsConfirmed: true)
            report.append("ok: saved (\(receipt.missingCredentialIDs.count) missing keys)")
            for saved in try await store.load() {
                for binding in saved.bindings {
                    do {
                        let read = try vault.publicIdentity(account: binding.account)
                        report.append(read?.fingerprint == identity.fingerprint ? "ok: read back" : "FAILED: read back a different key")
                    } catch { report.append("FAILED: read back: \(describe(error))") }
                    // A connection's key selection, which must not prompt.
                    let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
                        credentialAlgorithms: [identity.algorithm], hostKeyAlgorithms: [identity.algorithm])
                    do {
                        for issue in try await store.loadWithReadiness(support: support).flatMap({ $0.routes.flatMap(\.issues) }) {
                            report.append("route not ready: \(issue.message)")
                        }
                        let authorization = try await store.authorizeSSHAuthentication(workspaceID: saved.id,
                            expectedRevision: saved.revision, routeID: "route", credentialID: binding.credentialID,
                            presentedHostKey: .init(algorithm: identity.algorithm, fingerprint: identity.fingerprint), support: support)
                        let blob = try await AppleSSHCredentialSigner().publicIdentity(requestID: UUID(), authorization: authorization)
                        report.append(blob == identity.blob ? "ok: signer selects the key" : "FAILED: signer selected a different key")
                    } catch { report.append("FAILED: signer: \(describe(error))") }
                    do { try vault.remove(account: binding.account); report.append("ok: removed") }
                    catch { report.append("FAILED: remove: \(describe(error))") }
                }
            }
        } catch {
            report.append("FAILED: \(describe(error))")
        }
        return report
    }

    private static func describe(_ error: any Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? String(describing: error)
    }

    private static func throwawayKey(using crypto: AppleCryptography) throws -> (PrivateKeyRecord, SSHPublicIdentity) {
        let signing = Curve25519.Signing.PrivateKey()
        let seed = signing.rawRepresentation, publicKey = signing.publicKey.rawRepresentation
        func string(_ bytes: Data) -> Data {
            var result = Data(); result.appendBE(UInt64(bytes.count), bytes: 4); return result + bytes
        }
        let algorithm = Data("ssh-ed25519".utf8)
        let publicBlob = string(algorithm) + string(publicKey)
        let check = UInt64(UInt32.random(in: 0...UInt32.max))
        var privatePart = Data(); privatePart.appendBE(check, bytes: 4); privatePart.appendBE(check, bytes: 4)
        privatePart += string(algorithm) + string(publicKey) + string(seed + publicKey) + string(Data("self-test".utf8))
        var padding: UInt8 = 1
        while !privatePart.count.isMultiple(of: 16) { privatePart.append(padding); padding += 1 }
        let key = try PrivateKeyRecord.fromDecryptedOpenSSH(publicBlob: publicBlob, privatePart: privatePart,
                                                            credentialID: "key", using: crypto)
        return (key, try SSHPublicIdentity(blob: publicBlob))
    }

    private static func workspace(_ identity: SSHPublicIdentity) -> [String: Any] {
        let theme: [String: Any] = ["id": "theme", "name": "Self Test", "foreground": "#FFFFFF", "background": "#000000",
            "accent": "#3B82F6", "ansi": Array(repeating: "#808080", count: 16), "cursor_foreground": "#000000",
            "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF", "selection_foreground": "#FFFFFF",
            "selection_background": "#333333"]
        let workspace: [String: Any] = [
            "id": "storage-self-test", "name": "Storage self-test",
            "groups": [["id": "group", "name": "Self test", "machine_order": ["machine"]]],
            "machines": [["id": "machine", "name": "Self test", "theme_ref": "theme", "route_refs": ["route"],
                          "preferred_route_ref": "route", "auto_connect": false]],
            "routes": [["id": "route", "mode": "direct_ssh", "host": "127.0.0.1", "port": 22, "user": "self-test",
                        "credential_refs": ["key"], "authentication_methods": ["publickey"],
                        "host_trust_ref": "trust", "jump_routes": [], "session_creation": "on_select"]],
            "credentials": [["id": "key", "algorithm": identity.algorithm, "public_fingerprint": identity.fingerprint,
                             "requirement": "matching_private_key", "private_material": "not_included"]],
            "host_trust": [["id": "trust", "host": "127.0.0.1", "port": 22, "revoked_keys": [],
                            "pins": [["algorithm": identity.algorithm, "fingerprint": identity.fingerprint]]]],
            "themes": [theme],
            "preferences": ["initial_machine_ref": "machine", "initial_group_ref": "group", "connect_on_open": false]]
        return ["format": "ftworkspace", "version": 1, "bundle_id": UUID().uuidString,
                "source": ["exporter_id": "storage-self-test"], "required_capabilities": [], "workspace": workspace]
    }
}
#endif
