import Foundation
import XCTest
@testable import FrankenTermKit

final class AirGapMetadataTests: XCTestCase {
    private let crypto = TestCryptography()
    private let omittedSetting = "frankenterm.discarded_annotations"
    private let omittedReason = "Unsupported desktop annotations were omitted from public storage."

    private func archive(portability: [AirGapArchive.Portability] = [], annotations: [AirGapArchive.Annotation] = []) throws -> AirGapArchive {
        try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [],
                          portability: portability, annotations: annotations, using: crypto)
    }

    /// Encode untrusted tuples directly so rejection tests cannot accidentally
    /// pass only because the exporting initializer already sanitized their data.
    private func wireArchive(portability: [AirGapArchive.Portability] = [], annotations: [AirGapArchive.Annotation] = []) throws -> Data {
        var data = CBORWriter.array(5) + CBORWriter.uint(1) + CBORWriter.bytes(try sampleManifest()) + CBORWriter.array(0)
        data += CBORWriter.array(portability.count)
        for item in portability {
            data += CBORWriter.array(4) + CBORWriter.text(item.entityID) + CBORWriter.text(item.setting)
                + CBORWriter.uint(item.disposition) + CBORWriter.text(item.reason)
        }
        data += CBORWriter.array(annotations.count)
        for item in annotations {
            data += CBORWriter.array(3) + CBORWriter.text(item.entityID) + CBORWriter.text(item.setting) + CBORWriter.bytes(item.original)
        }
        return data
    }

    func testEveryEntityKindCanOwnAnInertAnnotationAndARequiredFinding() throws {
        let ids = ["work", "group", "build", "gpu", "route", "key", "trust", "theme"]
        let findings = ids.map { AirGapArchive.Portability(entityID: $0, setting: "desktop.policy", disposition: 1, reason: "Review this required desktop policy.") }
        let annotations = ids.map { AirGapArchive.Annotation(entityID: $0, setting: "enable_tab_bar", original: Data("true".utf8)) }
        let restored = try AirGapArchive(data: wireArchive(portability: findings, annotations: annotations), using: crypto)
        XCTAssertEqual(restored.portability, findings)
        XCTAssertEqual(restored.annotations, annotations)
        XCTAssertEqual(try archive(portability: findings, annotations: annotations).encoded(), try restored.encoded())
    }

    func testDanglingOrInvalidReferencesAndDuplicateMetadataRejectOnBothEntryPaths() throws {
        for id in ["missing", "", "work/route", String(repeating: "x", count: 129)] {
            let finding = AirGapArchive.Portability(entityID: id, setting: "desktop.policy", disposition: 1, reason: "Required policy.")
            let annotation = AirGapArchive.Annotation(entityID: id, setting: "enable_tab_bar", original: Data("true".utf8))
            XCTAssertThrowsError(try archive(portability: [finding]))
            XCTAssertThrowsError(try AirGapArchive(data: wireArchive(portability: [finding]), using: crypto))
            XCTAssertThrowsError(try archive(annotations: [annotation]))
            XCTAssertThrowsError(try AirGapArchive(data: wireArchive(annotations: [annotation]), using: crypto))
        }
        let finding = AirGapArchive.Portability(entityID: "route", setting: "desktop.policy", disposition: 1, reason: "Required policy.")
        let annotation = AirGapArchive.Annotation(entityID: "theme", setting: "enable_tab_bar", original: Data("true".utf8))
        XCTAssertThrowsError(try archive(portability: [finding, finding]))
        XCTAssertThrowsError(try AirGapArchive(data: wireArchive(portability: [finding, finding]), using: crypto))
        XCTAssertThrowsError(try archive(annotations: [annotation, annotation]))
        XCTAssertThrowsError(try AirGapArchive(data: wireArchive(annotations: [annotation, annotation]), using: crypto))
    }

    func testSettingsReasonsAndDispositionsAreBoundedAndValidated() throws {
        for setting in ["", "desktop\npolicy", "window opacity", "private/setting", "é", String(repeating: "x", count: 129)] {
            let finding = AirGapArchive.Portability(entityID: "route", setting: setting, disposition: 0, reason: "Inactive on this platform.")
            let annotation = AirGapArchive.Annotation(entityID: "theme", setting: setting, original: Data("true".utf8))
            XCTAssertThrowsError(try archive(portability: [finding]))
            XCTAssertThrowsError(try AirGapArchive(data: wireArchive(portability: [finding]), using: crypto))
            XCTAssertThrowsError(try archive(annotations: [annotation]))
            XCTAssertThrowsError(try AirGapArchive(data: wireArchive(annotations: [annotation]), using: crypto))
        }
        for reason in ["", "text\ncommand", "text\u{7F}", "text\u{85}", String(repeating: "a", count: FTLimit.text + 1)] {
            let finding = AirGapArchive.Portability(entityID: "route", setting: "desktop.policy", disposition: 0, reason: reason)
            XCTAssertThrowsError(try archive(portability: [finding]))
            XCTAssertThrowsError(try AirGapArchive(data: wireArchive(portability: [finding]), using: crypto))
        }
        let invalid = AirGapArchive.Portability(entityID: "route", setting: "desktop.policy", disposition: 2, reason: "Inactive.")
        XCTAssertThrowsError(try archive(portability: [invalid]))
        XCTAssertThrowsError(try AirGapArchive(data: wireArchive(portability: [invalid]), using: crypto))
        let oversized = AirGapArchive.Annotation(entityID: "work", setting: "unknown.setting", original: Data(repeating: 97, count: FTLimit.text + 1))
        let malformedUTF8 = AirGapArchive.Annotation(entityID: "work", setting: "unknown.setting", original: Data([0xFF]))
        for item in [oversized, malformedUTF8] {
            XCTAssertThrowsError(try archive(annotations: [item]))
            XCTAssertThrowsError(try AirGapArchive(data: wireArchive(annotations: [item]), using: crypto))
        }
    }

    func testRegisteredAppearanceValuesRemainInertAndRoundTripWithCanonicalPublicValues() throws {
        let annotations = [
            AirGapArchive.Annotation(entityID: "work", setting: "enable_tab_bar", original: Data(" \ntrue\t".utf8)),
            .init(entityID: "work", setting: "hide_tab_bar_if_only_one_tab", original: Data("false".utf8)),
            .init(entityID: "work", setting: "use_fancy_tab_bar", original: Data("true".utf8)),
            .init(entityID: "theme", setting: "window_background_opacity", original: Data("0.75000e0".utf8)),
            .init(entityID: "theme", setting: "text_background_opacity", original: Data("1.0".utf8))
        ]
        let finding = AirGapArchive.Portability(entityID: "theme", setting: "window_background_opacity", disposition: 1, reason: "A required desktop appearance mapping remains unresolved.")
        let decoded = try AirGapArchive(data: wireArchive(portability: [finding], annotations: annotations), using: crypto)
        XCTAssertEqual(decoded.annotations.map { String(decoding: $0.original, as: UTF8.self) }, ["true", "false", "true", "0.75", "1"])
        XCTAssertEqual(decoded.portability, [finding], "Preserving a value must not resolve its required behavior.")
        let exported = try FTAG.seal(decoded, using: crypto)
        let restored = try FTAG.open(exported.envelope, code: exported.code, using: crypto)
        XCTAssertEqual(restored.annotations, decoded.annotations)
        XCTAssertEqual(restored.portability, decoded.portability)
        XCTAssertEqual(restored.workspace.raw, try sampleManifest())
    }

    func testRegisteredSettingsRejectOpaqueNestedOrWrongTypedValues() throws {
        for original in [#""true""#, "1", "null", #"{"private_key":"synthetic-private-sentinel"}"#, "[]", "true false"] {
            let annotation = AirGapArchive.Annotation(entityID: "work", setting: "enable_tab_bar", original: Data(original.utf8))
            XCTAssertThrowsError(try archive(annotations: [annotation]))
            XCTAssertThrowsError(try AirGapArchive(data: wireArchive(annotations: [annotation]), using: crypto))
        }
        for original in [#""0.5""#, "true", "null", "[]", "-0.1", "1.1", "1e99999", "NaN", "Infinity"] {
            let annotation = AirGapArchive.Annotation(entityID: "theme", setting: "text_background_opacity", original: Data(original.utf8))
            XCTAssertThrowsError(try archive(annotations: [annotation]))
            XCTAssertThrowsError(try AirGapArchive(data: wireArchive(annotations: [annotation]), using: crypto))
        }
    }

    @MainActor
    func testOpaqueOptionalAnnotationNeverEntersReexportOrPublicCatalog() async throws {
        let syntheticSecret = Data("-----BEGIN OPENSSH PRIVATE KEY-----\nSYNTHETIC_PRIVATE_SENTINEL_FOR_REJECTION\n-----END OPENSSH PRIVATE KEY-----".utf8)
        let annotation = AirGapArchive.Annotation(entityID: "work", setting: "opaque_private_setting", original: syntheticSecret)
        let decoded = try AirGapArchive(data: wireArchive(annotations: [annotation]), using: crypto)
        XCTAssertTrue(decoded.annotations.isEmpty)
        XCTAssertEqual(decoded.portability, [.init(entityID: "work", setting: omittedSetting, disposition: 0, reason: omittedReason)])
        let safe = try decoded.encoded()
        XCTAssertNil(safe.range(of: syntheticSecret))
        XCTAssertNil(safe.range(of: Data(annotation.setting.utf8)))
        XCTAssertFalse(String(reflecting: annotation).contains("SYNTHETIC_PRIVATE_SENTINEL"))
        let reloaded = try AirGapArchive(data: safe, using: crypto)
        XCTAssertEqual(reloaded.portability, decoded.portability)
        XCTAssertEqual(try reloaded.encoded(), safe, "Omission findings must not accumulate on re-export.")

        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        let installed = try await store.install(decoded, envelopeDigest: crypto.sha256(safe), transferID: Data(repeating: 3, count: 16),
                                                expectedRevision: nil, sourceAndContentsConfirmed: true)
        XCTAssertTrue(installed.workspace.inactiveSettings.isEmpty)
        let catalog = try XCTUnwrap(persistence.bytes(.catalog))
        XCTAssertNil(catalog.range(of: syntheticSecret))
        XCTAssertNil(catalog.range(of: Data(syntheticSecret.base64EncodedString().utf8)))
        XCTAssertNil(catalog.range(of: Data(annotation.setting.utf8)))
        let reopened = WorkspaceStore(persistence: persistence, vault: vault)
        let saved = try await reopened.load()
        XCTAssertEqual(saved.count, 1)
        XCTAssertTrue(saved[0].inactiveSettings.isEmpty)
        XCTAssertEqual(saved[0].findings.map(\.reason), [omittedReason])
        XCTAssertEqual(vault.creates, 0)
    }

    func testRequiredOpaqueAnnotationsRejectWithoutErasingUnresolvedBehavior() throws {
        let required = AirGapArchive.Portability(entityID: "route", setting: "desktop.policy", disposition: 1, reason: "This policy must be satisfied before connecting.")
        let annotation = AirGapArchive.Annotation(entityID: "route", setting: "desktop.policy", original: Data("unrecognized policy payload".utf8))
        XCTAssertThrowsError(try archive(portability: [required], annotations: [annotation])) {
            XCTAssertEqual($0 as? FTError, .unsupported("annotation.required_setting"))
        }
        XCTAssertThrowsError(try AirGapArchive(data: wireArchive(portability: [required], annotations: [annotation]), using: crypto)) {
            XCTAssertEqual($0 as? FTError, .unsupported("annotation.required_setting"))
        }
        let retained = try archive(portability: [required])
        XCTAssertEqual(try AirGapArchive(data: retained.encoded(), using: crypto).portability, [required])
        let unrelated = AirGapArchive.Annotation(entityID: "theme", setting: "desktop.policy", original: Data("unrecognized optional value".utf8))
        let restored = try archive(portability: [required], annotations: [unrelated])
        XCTAssertEqual(restored.portability.first, required)
        XCTAssertEqual(restored.portability.count, 2)
    }

    func testMetadataCountsAndReservedOmissionFindingCannotBypassLimits() throws {
        let records = (0..<1024).map { AirGapArchive.Portability(entityID: "work", setting: "desktop.setting.\($0)", disposition: 0, reason: "Inactive.") }
        let unknown = AirGapArchive.Annotation(entityID: "work", setting: "unknown.setting", original: Data("opaque".utf8))
        XCTAssertEqual(try archive(portability: records).portability.count, 1024)
        XCTAssertThrowsError(try archive(portability: records, annotations: [unknown]))
        XCTAssertThrowsError(try AirGapArchive(data: wireArchive(portability: records, annotations: [unknown]), using: crypto))
        XCTAssertThrowsError(try archive(portability: records + [records[0]]))
        XCTAssertThrowsError(try archive(annotations: Array(repeating: unknown, count: 1025)))
        let valid = AirGapArchive.Portability(entityID: "work", setting: omittedSetting, disposition: 0, reason: omittedReason)
        XCTAssertEqual(try archive(portability: [valid], annotations: [unknown]).portability, [valid])
        for invalid in [
            AirGapArchive.Portability(entityID: "route", setting: omittedSetting, disposition: 0, reason: omittedReason),
            .init(entityID: "work", setting: omittedSetting, disposition: 1, reason: omittedReason),
            .init(entityID: "work", setting: omittedSetting, disposition: 0, reason: "untrusted diagnostic content")
        ] {
            XCTAssertThrowsError(try archive(portability: [invalid]))
            XCTAssertThrowsError(try AirGapArchive(data: wireArchive(portability: [invalid]), using: crypto))
        }
    }
}
