import Foundation
import XCTest
@testable import FrankenTermKit

final class OpticalPresentationTests: XCTestCase {
    let crypto = TestCryptography()
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
}

#if canImport(CoreImage) && canImport(Vision) && canImport(CryptoKit)
import ImageIO
import Vision

@MainActor
final class AppleOpticalPresentationTests: XCTestCase {
    func testNativeQRAndSavedAnimationDecodeBackIntoTheSameEnvelope() async throws {
        let crypto = AppleCryptography()
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        let exported = try FTAG.seal(archive, using: crypto)
        let sender = try OpticalSender(envelope: exported.envelope, using: crypto)
        let renderer = NativeOpticalPresentation(), text = try sender.frame(page: 0, sequence: 1, using: crypto)
        let symbol = try await renderer.symbol(text: text)
        let source = try XCTUnwrap(CGImageSourceCreateWithData(symbol.png as CFData, nil))
        let image = try XCTUnwrap(CGImageSourceCreateImageAtIndex(source, 0, nil))
        XCTAssertEqual(image.width, symbol.moduleSide)
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
