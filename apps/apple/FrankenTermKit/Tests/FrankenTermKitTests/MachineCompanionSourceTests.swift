import Foundation
import XCTest
@testable import FrankenTermKit

final class MachineCompanionSourceTests: XCTestCase {
    private let crypto = TestCryptography()

    private func finding(_ entity: String, _ setting: String, _ reason: String,
                         disposition: Int = 0) -> [String: Any] {
        ["entity_id": entity, "setting": setting, "reason": reason, "disposition": disposition]
    }

    private var scope: [String: Any] {
        finding("work", MachineCompanionSource.scopeSetting, "Inventory settings only; live overrides are not captured.")
    }

    private func desktopResponse(mutate: (inout [String: Any]) -> Void = { _ in }) throws -> Data {
        let instance = String(repeating: "a", count: 32)
        let revision = "desktop:\(instance):7:sha256:" + String(repeating: "b", count: 64)
        return try response(version: 3) { root in
            var document = root["document"] as! [String: Any]
            var source = document["source"] as! [String: Any]
            source["kind"] = "desktop_snapshot"
            source["configuration_revision"] = revision
            document["source"] = source; root["document"] = document
            root["runtime"] = ["instance_id": instance, "configuration_generation": 7,
                               "configuration_revision": revision] as [String: Any]
            root["portability"] = [finding("work", MachineCompanionSource.desktopScopeSetting,
                "Loaded inventory at one desktop generation; window overrides and live panes are not captured.")]
            mutate(&root)
        }
    }

    func testDesktopHandoffRetainsExactRuntimeRevisionThroughSelectionAndEncryptedRestore() throws {
        let source = try MachineCompanionSource(data: desktopResponse(), requireRunningDesktop: true)
        let runtime = try XCTUnwrap(source.runtime)
        XCTAssertEqual(runtime.configurationGeneration, 7)
        XCTAssertEqual(runtime.configurationRevision, source.workspace.configurationRevision)
        XCTAssertEqual(source.workspace.sourceKind, "desktop_snapshot")
        let selection = try WorkspaceExportSelection(workspace: source.workspace,
            groupIDs: ["group"], machineIDs: ["build"], findings: source.findings)
        let fingerprint = try XCTUnwrap(selection.identities.first?.fingerprint)
        XCTAssertFalse(try selection.review(decisions: [:], loadedFingerprints: []).canPrepare,
                       "source capture alone is not a private-key inclusion decision")
        let archive = try selection.archive(decisions: [fingerprint: .keepReferenceOnly], keys: [],
            bundleID: "running-desktop-selection", using: crypto)
        let review = try selection.review(decisions: [fingerprint: .keepReferenceOnly], loadedFingerprints: [])
        let sealed = try FTAG.seal(archive, using: crypto)
        let restored = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(restored.workspace.configurationRevision, runtime.configurationRevision)
        XCTAssertEqual(restored.workspace.machines.map(\.id), ["build"])
        XCTAssertEqual(restored.portability, review.findings)
        XCTAssertTrue(source.findings.allSatisfy(restored.portability.contains),
                      "source scope travels together with the reviewed missing-key/readiness findings")
        XCTAssertNil(restored.workspace.raw.range(of: Data("/source/ssh/selected_key".utf8)))
    }

    func testRunningHandoffNeverAcceptsAnInventoryFallbackOrMismatchedWitness() throws {
        for version in [1, 2] {
            XCTAssertThrowsError(try MachineCompanionSource(data: response(version: version), requireRunningDesktop: true)) {
                XCTAssertEqual($0 as? FTError, .unsupported("machines.running_source_required"))
            }
        }
        let mutations: [(inout [String: Any]) -> Void] = [
            { $0.removeValue(forKey: "runtime") },
            { $0["runtime"] = ["instance_id": "copied"] },
            { root in var runtime = root["runtime"] as! [String: Any]; runtime["configuration_generation"] = 8; root["runtime"] = runtime },
            { root in var runtime = root["runtime"] as! [String: Any]; runtime["configuration_generation"] = 0; root["runtime"] = runtime },
            { root in var runtime = root["runtime"] as! [String: Any]; runtime["instance_id"] = String(repeating: "A", count: 32); root["runtime"] = runtime },
            { root in var runtime = root["runtime"] as! [String: Any]; runtime["extra"] = true; root["runtime"] = runtime },
            { root in var document = root["document"] as! [String: Any]; var source = document["source"] as! [String: Any]; source["configuration_revision"] = "different"; document["source"] = source; root["document"] = document },
            { root in var document = root["document"] as! [String: Any]; var source = document["source"] as! [String: Any]; source["kind"] = "machines_toml"; document["source"] = source; root["document"] = document },
            { $0["portability"] = [self.scope] },
            { $0["portability"] = [self.finding("work", MachineCompanionSource.desktopScopeSetting, "Wrong disposition", disposition: 1)] }
        ]
        for mutation in mutations {
            XCTAssertThrowsError(try MachineCompanionSource(data: desktopResponse(mutate: mutation), requireRunningDesktop: true))
        }
        XCTAssertThrowsError(try MachineCompanionSource(data: desktopResponse { root in
            root["version"] = 2; root.removeValue(forKey: "runtime"); root["portability"] = [self.scope]
        }))
    }

    func testRuntimeDuplicateKeysCannotChooseOneOfTwoSourceGenerations() throws {
        let bytes = try desktopResponse()
        let text = String(decoding: bytes, as: UTF8.self)
        let duplicate = text.replacingOccurrences(of: "\"configuration_generation\":7",
            with: "\"configuration_generation\":7,\"configuration_generation\":8")
        XCTAssertNotEqual(text, duplicate)
        XCTAssertThrowsError(try MachineCompanionSource(data: Data(duplicate.utf8), requireRunningDesktop: true)) {
            XCTAssertEqual($0 as? FTError, .malformed("json.duplicate_key"))
        }
    }

    private func response(document: Data? = nil, version: Int = 2,
                          mutate: (inout [String: Any]) -> Void = { _ in }) throws -> Data {
        let manifest = try document ?? sampleManifest()
        var root: [String: Any] = [
            "format": "ftmachines-source", "version": version, "inventory": "/source/machines.toml",
            "document": try JSONSerialization.jsonObject(with: manifest),
            "identity_files": ["key": "/source/ssh/selected_key"]
        ]
        if version == 1 { root["notes"] = [] as [String] }
        else { root["portability"] = [scope] }
        mutate(&root)
        return try JSONSerialization.data(withJSONObject: root, options: [.sortedKeys])
    }

    func testDefaultHelperFindingsReachSelectedQRArchiveAndInstalledWorkspace() async throws {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let raw = try response(document: sampleManifest(fingerprint: fingerprint)) { root in
            root["portability"] = [
                scope,
                finding("group", "machines_toml.omitted_machine.hidden", "hidden: no portable route; machine omitted"),
                finding("build", "machines_toml.omitted_route.proxy", "build: executable proxy route omitted"),
                finding("gpu", "machines_toml.source_note.gradient", "gpu: desktop gradient remains inactive"),
                finding("work", "machines_toml.omitted_machine.entire_group", "An unavailable group has no retained machines.")
            ]
        }
        let source = try MachineCompanionSource(data: raw)
        XCTAssertEqual(source.identityFiles, ["key": "/source/ssh/selected_key"])
        let selection = try WorkspaceExportSelection(workspace: source.workspace,
            groupIDs: ["group"], machineIDs: ["build"], findings: source.findings)
        let review = try selection.review(decisions: [fingerprint: .includePrivateKey], loadedFingerprints: [fingerprint])
        XCTAssertTrue(review.canPrepare)
        XCTAssertFalse(review.findings.contains { $0.entityID == "gpu" }, "excluded machine's own setting is irrelevant")
        XCTAssertEqual(Set(review.findings.map(\.entityID)), ["work", "group", "build"])
        XCTAssertEqual(review.findings.filter { $0.setting.contains("omitted_machine") }.count, 2,
                       "entities removed before helper output remain visible through surviving group/workspace scope")
        let archive = try selection.archive(decisions: [fingerprint: .includePrivateKey], keys: [key],
            bundleID: "helper-selected-transfer", using: crypto)
        let sealed = try FTAG.seal(archive, using: crypto)
        let sender = try OpticalSender(envelope: sealed.envelope, using: crypto)
        let first = try OpticalFrame(text: sender.frame(page: 0, sequence: 1, using: crypto))
        var receiver = OpticalReceiver(selecting: first)
        for page in (0..<sender.identity.pageCount).reversed() {
            for sequence in (1...(try sender.sourcePartCount(page: page))).reversed() {
                try receiver.receive(OpticalFrame(text: sender.frame(page: page, sequence: UInt32(sequence), using: crypto)), using: crypto)
            }
        }
        let envelope = try receiver.envelope(using: crypto)
        let restored = try FTAG.open(envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(restored.portability, review.findings)
        XCTAssertEqual(restored.workspace.machines.map(\.id), ["build"])
        let store = WorkspaceStore(persistence: StoreTestPersistence(), vault: StoreTestVault())
        let receipt = try await store.install(restored, envelopeDigest: crypto.sha256(envelope),
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        XCTAssertEqual(receipt.workspace.findings.map(\.reason), review.findings.map(\.reason))
        XCTAssertEqual(receipt.workspace.findings.map(\.entityID), review.findings.map(\.entityID))
        XCTAssertNil(restored.workspace.raw.range(of: Data("/source/ssh/selected_key".utf8)),
                     "local key-file hints do not enter the public manifest")
    }

    func testLegacyHelperNotesSurviveSelectionInsteadOfDisappearing() throws {
        let notes = ["offline: no exportable route", "build: a required source setting is unresolved"]
        let source = try MachineCompanionSource(data: response(version: 1) { $0["notes"] = notes })
        XCTAssertEqual(source.findings.dropFirst().map(\.reason), notes)
        XCTAssertTrue(source.findings.allSatisfy { $0.entityID == source.workspace.id })
        let selection = try WorkspaceExportSelection(workspace: source.workspace,
            groupIDs: ["group"], machineIDs: ["build"], findings: source.findings)
        let fingerprint = try XCTUnwrap(selection.identities.first?.fingerprint)
        let archive = try selection.archive(decisions: [fingerprint: .keepReferenceOnly], keys: [],
            bundleID: "legacy-helper-transfer", using: crypto)
        XCTAssertTrue(notes.allSatisfy { note in archive.portability.contains { $0.reason == note } })
    }

    func testSourceNotesDoNotEraseRequiredRoutePolicyOrInventAnAllWorkspaceDenial() throws {
        var document = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest()) as? [String: Any])
        var workspace = try XCTUnwrap(document["workspace"] as? [String: Any])
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        routes[0]["required_capabilities"] = ["ssh_config.unresolved_policy"]
        workspace["routes"] = routes; document["workspace"] = workspace
        let source = try MachineCompanionSource(data: response(document: JSONSerialization.data(withJSONObject: document)) {
            $0["portability"] = [scope, finding("route", "machines_toml.source_note.policy", "Required SSH source policy remains unresolved.")]
        })
        let selection = try WorkspaceExportSelection(workspace: source.workspace, groupIDs: ["group"], findings: source.findings)
        let fingerprint = try XCTUnwrap(selection.identities.first?.fingerprint)
        let review = try selection.review(decisions: [fingerprint: .keepReferenceOnly], loadedFingerprints: [])
        XCTAssertEqual(review.blockedRouteIDs, ["route"])
        XCTAssertTrue(source.findings.allSatisfy { $0.disposition == 0 })
        XCTAssertTrue(review.findings.contains { $0.entityID == "route" && $0.disposition == 1 })
        let publicDocument = try selection.publicDocument(bundleID: "public-helper-settings")
        XCTAssertEqual(publicDocument.routes[0].requiredCapabilities, ["ssh_config.unresolved_policy"])
    }

    func testMalformedOrIncompleteReportsRefuseBeforeAnyUsableSourceIsReturned() throws {
        let mutations: [(inout [String: Any]) -> Void] = [
            { $0.removeValue(forKey: "portability") },
            { $0["portability"] = [] as [[String: Any]] },
            { $0["portability"] = [self.scope, self.scope] },
            { $0["portability"] = [self.scope, self.finding("absent", "source.policy", "Missing entity")] },
            { $0["portability"] = [self.scope, self.finding("route", "source.policy", "Unknown disposition", disposition: 2)] },
            { $0["portability"] = [self.scope, self.finding("route", "source.policy", "embedded\ncontrol")] },
            { $0["portability"] = [self.scope, self.finding("route", "source.policy", "")] },
            { $0["identity_files"] = ["absent": "/source/key"] },
            { $0["identity_files"] = ["key": "relative-key"] },
            { $0["identity_files"] = ["key": "/source/$EXPANSION"] },
            { $0["identity_files"] = ["key": "/source/key\u{7f}"] },
            { $0["version"] = 4 }
        ]
        for mutate in mutations {
            XCTAssertThrowsError(try MachineCompanionSource(data: response(mutate: mutate)))
        }
        XCTAssertThrowsError(try MachineCompanionSource(data: response(version: 1) { $0.removeValue(forKey: "notes") }))
        XCTAssertThrowsError(try MachineCompanionSource(data: response(version: 1) {
            $0["notes"] = ["first valid note", 7] as [Any]
        }))
    }

    func testDuplicateJSONKeysAreRejectedAtEveryBridgeLevel() throws {
        let valid = try response()
        let text = try XCTUnwrap(String(data: valid, encoding: .utf8))
        let duplicateVersion = text.replacingOccurrences(of: "\"version\":2", with: "\"version\":2,\"version\":1")
        let duplicateInventory = text.replacingOccurrences(of: "\"inventory\":", with: "\"inventory\":\"first\",\"inventory\":")
        let duplicateReason = text.replacingOccurrences(of: "\"reason\":", with: "\"reason\":\"first\",\"reason\":")
        let duplicateDocument = text.replacingOccurrences(of: "\"bundle_id\":", with: "\"bundle_id\":\"first\",\"bundle_id\":")
        for invalid in [duplicateVersion, duplicateInventory, duplicateReason, duplicateDocument] {
            XCTAssertNotEqual(invalid, text)
            XCTAssertThrowsError(try MachineCompanionSource(data: Data(invalid.utf8))) {
                XCTAssertEqual($0 as? FTError, .malformed("json.duplicate_key"))
            }
        }
    }

    func testCountTextAndAggregateBudgetsNeverReturnAUsablePrefix() throws {
        let exact = [scope] + (0..<1023).map { finding("work", "source.note.\($0)", "One source setting remains inactive.") }
        XCTAssertEqual(try MachineCompanionSource(data: response { $0["portability"] = exact }).findings.count, 1024)
        XCTAssertThrowsError(try MachineCompanionSource(data: response {
            $0["portability"] = exact + [self.finding("work", "source.note.extra", "Over count")]
        }))
        XCTAssertThrowsError(try MachineCompanionSource(data: response {
            $0["portability"] = [self.scope, self.finding("work", "source.note.large", String(repeating: "x", count: FTLimit.text + 1))]
        }))
        XCTAssertThrowsError(try MachineCompanionSource(data: response {
            $0["portability"] = [self.scope] + (0..<70).map {
                self.finding("work", "source.note.\($0)", String(repeating: "x", count: FTLimit.text))
            }
        })) { XCTAssertEqual($0 as? FTError, .limit("machines.source_findings")) }
        XCTAssertThrowsError(try MachineCompanionSource(data: Data(repeating: 32, count: MachineCompanionSource.maximumBytes + 1)))
    }

    func testNestedLargeCertificateKeepsItsExactExceptionWithoutExpandingOrdinaryText() throws {
        let fixture = try UserCertificateFixtures.load().largeCertificate
        let identity = try SSHPublicIdentity(blob: fixture.publicBlob)
        var document = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest()) as? [String: Any])
        var workspace = try XCTUnwrap(document["workspace"] as? [String: Any])
        var credentials = try XCTUnwrap(workspace["credentials"] as? [[String: Any]])
        credentials[0]["algorithm"] = identity.algorithm
        credentials[0]["public_fingerprint"] = identity.fingerprint
        credentials[0]["user_certificate"] = fixture.certificateBlob.base64EncodedString()
        credentials[0]["required_capabilities"] = [WorkspaceDocument.userCertificateCapability]
        workspace["credentials"] = credentials; document["workspace"] = workspace
        let manifest = try JSONSerialization.data(withJSONObject: document)
        let source = try MachineCompanionSource(data: response(document: manifest))
        XCTAssertEqual(source.workspace.credentials[0].userCertificate?.blob, fixture.certificateBlob)
        XCTAssertThrowsError(try MachineCompanionSource(data: response(document: manifest) {
            $0["portability"] = [self.scope, self.finding("work", "source.policy", fixture.certificateBlob.base64EncodedString())]
        }))
        let fake = ["document": ["workspace": ["credentials": [["user_certificate": String(repeating: "A", count: FTLimit.text + 1)]]]]]
        XCTAssertThrowsError(try StrictJSON.decode(JSONSerialization.data(withJSONObject: fake)),
                             "ordinary manifest decoding must not gain a wrapper exception")
    }
}
