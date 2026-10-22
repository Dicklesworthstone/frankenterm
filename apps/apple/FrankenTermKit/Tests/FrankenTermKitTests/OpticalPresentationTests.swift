import Foundation
import XCTest
@testable import FrankenTermKit

final class OpticalPresentationTests: XCTestCase {
    let crypto = TestCryptography()
    func testExportPlaybackReconstructsAndUnlocksOnlyWithTheSeparatelyRevealedCode() throws {
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        let sealed = try FTAG.seal(archive, using: crypto)
        var session = try OpticalExportSession(export: sealed, using: crypto)
        XCTAssertTrue(session.hasTransferCode)
        XCTAssertEqual(session.state, .paused)
        let code = try session.revealTransferCode(using: crypto)
        XCTAssertEqual(code, sealed.code.reveal(using: crypto))
        XCTAssertEqual(session.state, .revealingCode)
        let playback = try session.beginPlayback()
        var receiver: OpticalReceiver?
        for _ in 0..<session.identity.sourcePartCount {
            let output = try session.next(playback, using: crypto)
            XCTAssertEqual(output.symbolsPerSecond, 5)
            XCTAssertNil(TransferCodeFrame.parse(output.text))
            XCTAssertFalse(output.text.contains(code))
            let frame = try OpticalFrame(text: output.text)
            if receiver == nil { receiver = OpticalReceiver(selecting: frame) }
            try receiver?.receive(frame, using: crypto)
        }
        let recovered = try XCTUnwrap(receiver?.envelope(using: crypto))
        XCTAssertEqual(recovered, sealed.envelope)
        XCTAssertEqual(try FTAG.open(recovered, code: TransferCode(entered: code, using: crypto), using: crypto).workspace.raw,
                       archive.workspace.raw)
        XCTAssertThrowsError(try FTAG.open(recovered, code: TransferCode(secret: Data(repeating: 0xFE, count: 16)), using: crypto))
        XCTAssertEqual(session.envelope, sealed.envelope)
        XCTAssertFalse(String(reflecting: session).contains(code))
        // Exercise the actual recording verifier with the session's saved
        // ciphertext after code reveal and live playback have both occurred.
        var video = try OpticalVideoVerifier(source: OpticalVideoSource(envelope: session.envelope),
            symbolsPerSecond: 10, using: crypto)
        for (index, text) in video.expectedTexts.enumerated() {
            XCTAssertNil(TransferCodeFrame.parse(text))
            XCTAssertFalse(text.contains(code))
            for hold in 0..<video.plan.framesPerSymbol {
                try video.receive(decodedText: text, presentationFrame: index * video.plan.framesPerSymbol + hold,
                    durationFrames: 1, using: crypto)
            }
        }
        try video.finish(using: crypto)
    }

    func testCodeRevealRetiresPlaybackAndOldCleanupCannotPauseAReplacement() throws {
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        let sealed = try FTAG.seal(archive, using: crypto)
        var session = try OpticalExportSession(export: sealed, using: crypto)
        let first = try session.beginPlayback()
        let firstFrame = try session.next(first, using: crypto)
        _ = try session.revealTransferCode(using: crypto)
        XCTAssertThrowsError(try session.next(first, using: crypto)) { XCTAssertEqual($0 as? FTError, .stale) }
        session.pause(first)
        XCTAssertEqual(session.state, .revealingCode)
        let next = try session.beginPlayback(symbolsPerSecond: 3)
        session.pause(first)
        XCTAssertEqual(session.state, .playing)
        let secondFrame = try session.next(next, using: crypto)
        XCTAssertNotEqual(firstFrame.text, secondFrame.text)
        XCTAssertEqual(secondFrame.emittedSymbolCount, 2)
        XCTAssertEqual(secondFrame.symbolsPerSecond, 3)
        session.pause(next)
        XCTAssertThrowsError(try session.next(next, using: crypto))
        XCTAssertEqual(session.state, .paused)
        XCTAssertEqual(session.envelope, sealed.envelope)
    }

    func testReopenedEncryptedFileSupportsFreshTargetedRepairWithoutARecoveredCode() throws {
        var random = URRandom(seed: Data("reopened encrypted backup".utf8), using: crypto)
        let bytes = Data(try (0..<40000).map { _ in UInt8(try random.index(count: 256)) })
        let envelope = try FTAG.sealArchiveBytes(bytes, code: TransferCode(secret: Data(repeating: 8, count: 16)),
            transferID: Data(repeating: 7, count: 16), salt: Data(repeating: 6, count: 16),
            nonce: Data(repeating: 5, count: 12), compress: false, using: crypto)
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ft-reopen-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        let file = root.appendingPathComponent("saved.ftairgap")
        try envelope.write(to: file, options: .withoutOverwriting)
        var session = try OpticalExportSession(envelope: SelectedLocalFile.read(file, limit: FTLimit.envelope), using: crypto)
        XCTAssertFalse(session.hasTransferCode)
        XCTAssertThrowsError(try session.revealTransferCode(using: crypto))
        XCTAssertEqual(session.identity.pageCount, 2)
        let source = try OpticalSender(envelope: envelope, using: crypto)
        var receiver: OpticalReceiver?
        for page in [1, 0] {
            let playback = try session.beginPlayback(symbolsPerSecond: page == 1 ? 10 : 3, repairPage: page)
            for _ in 0..<(try source.sourcePartCount(page: page)) {
                let output = try session.next(playback, using: crypto)
                let frame = try OpticalFrame(text: output.text)
                XCTAssertEqual(frame.pageIndex, page)
                XCTAssertEqual(frame.identity, source.identity)
                if receiver == nil { receiver = OpticalReceiver(selecting: frame) }
                try receiver?.receive(frame, using: crypto)
            }
            session.pause(playback)
        }
        XCTAssertEqual(try receiver?.envelope(using: crypto), envelope)
        let repair = try session.beginPlayback(repairPage: 1)
        let frame = try OpticalFrame(text: session.next(repair, using: crypto).text)
        XCTAssertEqual(frame.pageIndex, 1)
        let parts = frame.ur.split(separator: "/")
        XCTAssertEqual(parts.count, 3)
        let part = try URPart(cbor: Bytewords.decode(parts[2]))
        XCTAssertGreaterThan(part.sequence, UInt32(try source.sourcePartCount(page: 1)))
        XCTAssertEqual(session.envelope, envelope)
        XCTAssertEqual(try Data(contentsOf: file), envelope)
    }

    func testPlaybackSettingsAndForeignLeasesCannotChangeTheSelectedExport() throws {
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        let sealed = try FTAG.seal(archive, using: crypto)
        var session = try OpticalExportSession(export: sealed, using: crypto)
        var other = try OpticalExportSession(envelope: sealed.envelope, using: crypto)
        let current = try session.beginPlayback(symbolsPerSecond: 10)
        let foreign = try other.beginPlayback()
        XCTAssertThrowsError(try session.next(foreign, using: crypto))
        session.pause(foreign)
        for rate in [0, 1, 4, 6, 30] { XCTAssertThrowsError(try session.beginPlayback(symbolsPerSecond: rate)) }
        for page in [-1, session.identity.pageCount] { XCTAssertThrowsError(try session.beginPlayback(repairPage: page)) }
        XCTAssertEqual(try session.next(current, using: crypto).symbolsPerSecond, 10)
        XCTAssertEqual(session.emittedSymbolCount, 1)
        XCTAssertThrowsError(try session.updatePlayback(foreign, symbolsPerSecond: 3, repairPage: 0))
        XCTAssertThrowsError(try session.updatePlayback(current, symbolsPerSecond: 3, repairPage: -1))
        XCTAssertEqual(try session.next(current, using: crypto).symbolsPerSecond, 10)
        try session.updatePlayback(current, symbolsPerSecond: 3, repairPage: 0)
        let repaired = try session.next(current, using: crypto)
        XCTAssertEqual(repaired.symbolsPerSecond, 3)
        XCTAssertEqual(try OpticalFrame(text: repaired.text).pageIndex, 0)
        XCTAssertEqual(session.envelope, sealed.envelope)
        XCTAssertNotEqual(session.id, other.id)
    }

    func testRecipientPresentationCannotRevealOrInventAGeneratedCode() throws {
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        let recipient = try RecipientSession(using: crypto)
        let envelope = try FTAG.seal(archive, recipient: recipient.request, using: crypto)
        var session = try OpticalExportSession(envelope: envelope, using: crypto)
        XCTAssertEqual(session.mode, .recipient)
        XCTAssertFalse(session.hasTransferCode)
        XCTAssertThrowsError(try session.revealTransferCode(using: crypto))
        let playback = try session.beginPlayback()
        let frame = try OpticalFrame(text: session.next(playback, using: crypto).text)
        XCTAssertEqual(frame.identity.envelopeDigest, crypto.sha256(envelope))
        XCTAssertEqual(try FTAG.open(session.envelope, recipient: recipient, using: crypto).workspace.raw, archive.workspace.raw)
        XCTAssertThrowsError(try OpticalExportSession(export: .init(envelope: envelope,
            code: TransferCode(secret: Data(repeating: 1, count: 16))), using: crypto))
        XCTAssertThrowsError(try OpticalExportSession(envelope: envelope + Data([0]), using: crypto))
        XCTAssertThrowsError(try OpticalExportSession(envelope: Data("not an envelope".utf8), using: crypto))
    }

    func testFiniteScheduleContainsEverySystematicPartAndReconstructs() throws {
        var random = URRandom(seed: Data("finite recording test".utf8), using: crypto)
        let data = Data(try (0..<40000).map { _ in UInt8(try random.index(count: 256)) })
        let envelope = try FTAG.sealArchiveBytes(data, code: TransferCode(secret: Data(repeating: 1, count: 16)),
            transferID: Data(repeating: 2, count: 16), salt: Data(repeating: 3, count: 16), nonce: Data(repeating: 4, count: 12), compress: false, using: crypto)
        let sender = try OpticalSender(envelope: envelope, using: crypto), plan = try OpticalAnimationPlan(sender: sender)
        XCTAssertEqual(sender.identity.pageCount, 2)
        var receiver: OpticalReceiver?
        for page in 0..<sender.identity.pageCount {
            let count = try sender.sourcePartCount(page: page)
            let numbers = plan.addresses.filter { $0.page == page }.map(\.sequence)
            XCTAssertEqual(numbers, Array(UInt32(1)...UInt32(count + (count + 1) / 2 + 16)))
        }
        XCTAssertEqual(plan.addresses.prefix(8).map(\.page), Array(repeating: 0, count: 8))
        XCTAssertEqual(plan.addresses.dropFirst(8).prefix(8).map(\.page), Array(repeating: 1, count: 8))
        for address in plan.addresses {
            let frame = try OpticalFrame(text: sender.frame(page: address.page, sequence: address.sequence, using: crypto))
            if receiver == nil { receiver = OpticalReceiver(selecting: frame) }
            try receiver?.receive(frame, using: crypto)
        }
        XCTAssertEqual(try receiver?.envelope(using: crypto), envelope)
        XCTAssertThrowsError(try OpticalAnimationPlan(sender: sender, maximumFrames: plan.addresses.count - 1), "never truncate a recording")
        XCTAssertThrowsError(try OpticalAnimationPlan(sender: sender, maximumFrames: 0))
        XCTAssertThrowsError(try OpticalAnimationPlan(sender: sender, maximumFrames: 4097))
    }
    func testSelectedLocalFileBoundsAndRegularFileRequirement() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ft-input-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        let file = root.appendingPathComponent("selected.ftairgap")
        try Data(repeating: 7, count: 129).write(to: file, options: .withoutOverwriting)
        XCTAssertEqual(try SelectedLocalFile.read(file, limit: 129), Data(repeating: 7, count: 129))
        XCTAssertThrowsError(try SelectedLocalFile.read(file, limit: 128))
        XCTAssertThrowsError(try SelectedLocalFile.read(root, limit: 1024))
        XCTAssertThrowsError(try SelectedLocalFile.read(URL(string: "https://example.invalid/export")!, limit: 1024))
        XCTAssertThrowsError(try SelectedLocalFile.read(file, limit: 0))
    }
    func testManifestPublishesTheExplicitInitialGroupRatherThanGuessingByMembership() throws {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest()) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        workspace["groups"] = [["id": "other", "name": "Other", "machine_order": ["gpu", "build"]],
                               ["id": "group", "name": "Selected", "machine_order": ["build", "gpu"]]]
        root["workspace"] = workspace
        let parsed = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        XCTAssertEqual(parsed.initialGroupID, "group")
        XCTAssertEqual(parsed.groups.first(where: { $0.id == parsed.initialGroupID })?.machineOrder, ["build", "gpu"])
    }
    func testExporterPreservesReviewedInactiveSettingsAndRequiredOmissions() throws {
        let workspace = try WorkspaceDocument(data: sampleManifest())
        let findings: [AirGapArchive.Portability] = [
            .init(entityID: "theme", setting: "window_background_opacity", disposition: 0, reason: "The desktop window opacity remains inactive on the phone."),
            .init(entityID: "route", setting: "required_revocation_check", disposition: 1, reason: "This check cannot run while offline.")
        ]
        let annotation = AirGapArchive.Annotation(entityID: "theme", setting: "window_background_opacity", original: Data("0.75".utf8))
        let archive = try AirGapArchive(workspace: workspace, keys: [], portability: findings, annotations: [annotation], using: crypto)
        let encrypted = try FTAG.seal(archive, using: crypto)
        let restored = try FTAG.open(encrypted.envelope, code: encrypted.code, using: crypto)
        XCTAssertEqual(restored.portability.map(\.disposition), [0, 1])
        XCTAssertEqual(restored.portability.map(\.entityID), ["theme", "route"])
        XCTAssertEqual(restored.annotations[0].original, annotation.original)
        XCTAssertEqual(restored.workspace.raw, workspace.raw)
        XCTAssertThrowsError(try AirGapArchive(workspace: workspace, keys: [], portability: findings + [findings[0]], using: crypto))
        XCTAssertThrowsError(try AirGapArchive(workspace: workspace, keys: [], portability: findings, annotations: [annotation, annotation], using: crypto))
        let opaque = AirGapArchive.Annotation(entityID: "theme", setting: "font", original: Data("Desktop Mono".utf8))
        let sanitized = try AirGapArchive(workspace: workspace, keys: [], annotations: [opaque], using: crypto)
        XCTAssertTrue(sanitized.annotations.isEmpty)
        XCTAssertEqual(sanitized.portability.map(\.setting), ["frankenterm.discarded_annotations"], "unknown values leave a content-free explanation")
        XCTAssertNil(try sanitized.encoded().range(of: opaque.original))
        for invalid in [
            AirGapArchive.Portability(entityID: "missing", setting: "font", disposition: 0, reason: "Not available"),
            .init(entityID: "theme", setting: "font", disposition: 2, reason: "Invalid disposition"),
            .init(entityID: "theme", setting: "font", disposition: 0, reason: ""),
            .init(entityID: "theme", setting: String(repeating: "x", count: FTLimit.text + 1), disposition: 0, reason: "Too large")
        ] {
            XCTAssertThrowsError(try AirGapArchive(workspace: workspace, keys: [], portability: [invalid], using: crypto))
        }
        // Decoder applies the same source-independent entity validation. These
        // bytes are syntactically canonical CBOR but carry a nonexistent target.
        var invalidCBOR = CBORWriter.array(5) + CBORWriter.uint(1) + CBORWriter.bytes(workspace.raw) + CBORWriter.array(0)
        invalidCBOR += CBORWriter.array(1) + CBORWriter.array(4) + CBORWriter.text("missing") + CBORWriter.text("font")
        invalidCBOR += CBORWriter.uint(0) + CBORWriter.text("Not available") + CBORWriter.array(0)
        XCTAssertThrowsError(try AirGapArchive(data: invalidCBOR, using: crypto))
    }
}

#if canImport(CoreImage) && canImport(Vision) && canImport(CryptoKit)
import ImageIO
import Vision

@MainActor
final class AppleOpticalPresentationTests: XCTestCase {
    func testNativeQRAndSavedAnimationDecodeBackIntoTheSameEnvelope() async throws {
        try skipIfSimulatorLacksVisionBarcodeInference()
        let crypto = AppleCryptography()
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        let exported = try FTAG.seal(archive, using: crypto)
        let sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        let renderer = NativeOpticalPresentation(), text = try sender.frame(page: 0, sequence: 1, using: crypto)
        let symbol = try await renderer.symbol(text: text)
        let source = try XCTUnwrap(CGImageSourceCreateWithData(symbol.png as CFData, nil))
        let image = try XCTUnwrap(CGImageSourceCreateImageAtIndex(source, 0, nil))
        XCTAssertEqual(image.width, symbol.moduleSide)
        XCTAssertEqual(image.width, 113)
        XCTAssertEqual(image.height, 113)
        let request = VNDetectBarcodesRequest()
        request.symbologies = [.qr]
        try VNImageRequestHandler(cgImage: image, options: [:]).perform([request])
        XCTAssertEqual(request.results?.map(\.payloadStringValue), [text])
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("ft-animation-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        let url = root.appendingPathComponent("test.apng")
        let count = try await renderer.saveAnimation(envelope: exported.envelope, to: url, symbolsPerSecond: 5)
        XCTAssertEqual(count, try OpticalAnimationPlan(sender: sender).addresses.count)
        let original = try Data(contentsOf: url)
        do { _ = try await renderer.saveAnimation(envelope: exported.envelope, to: url); XCTFail("overwrote existing backup") }
        catch {}
        XCTAssertEqual(try Data(contentsOf: url), original)
        XCTAssertNil(original.range(of: Data(exported.code.reveal(using: crypto).utf8)))
    }
}
#endif
