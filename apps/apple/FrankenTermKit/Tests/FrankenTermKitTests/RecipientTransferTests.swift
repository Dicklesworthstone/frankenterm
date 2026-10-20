import Foundation
import XCTest
@testable import FrankenTermKit

final class RecipientTransferTests: XCTestCase {
    let crypto = TestCryptography()

    func testRequestUsesExactCanonicalSinglePartURTranscript() throws {
        let requestID = Data((0..<16).map(UInt8.init))
        let publicKey = try Data(ftHex: "4310ee97d88cc1f088a5576c77ab0cf5c3ac797f3d95139c6c84b5429c59662a")
        let request = try RecipientRequest(requestID: requestID, publicKey: publicKey, maximumEnvelopeBytes: FTLimit.envelope)
        XCTAssertEqual(request.encoded.ftHex, "840150000102030405060708090A0B0C0D0E0F58204310EE97D88CC1F088A5576C77AB0CF5C3AC797F3D95139C6C84B5429C59662A1A00400000")
        XCTAssertEqual(request.encoded.count, 58)
        XCTAssertEqual(try RecipientRequest(text: request.text), request)
        XCTAssertTrue(request.text.hasPrefix("FTREQ1:UR:BYTES/"))
        XCTAssertEqual(request.text.split(separator: "/").count, 2)
        var outer = CBORReader(try Bytewords.decode(request.text.dropFirst("FTREQ1:UR:BYTES/".count)))
        XCTAssertEqual(try outer.bytes(cap: 4096), request.encoded)
        try outer.finish()
        XCTAssertEqual(request.comparison(using: crypto), FTAG.comparison(request.encoded, using: crypto))
    }

    func testRequestRejectsAmbiguousEncodingAndInvalidProfiles() throws {
        let request = try RecipientSession(using: crypto).request
        for text in [request.text.lowercased(), "https://example.invalid/" + request.text,
                     request.text + "\n", request.text + "/", request.text.replacingOccurrences(of: "UR:BYTES/", with: "UR:BYTES/1-1/"),
                     "FTREQ1:UR:BYTES/" + String(repeating: "AA", count: 2049)] {
            XCTAssertThrowsError(try RecipientRequest(text: text))
        }
        let fields = CBORWriter.bytes(request.requestID) + CBORWriter.bytes(request.publicKey)
        let maximum = CBORWriter.uint(UInt64(FTLimit.envelope))
        var invalidRequests: [Data] = []
        invalidRequests.append(Data([0x84, 0x18, 0x01]) + fields + maximum) // nonminimal version
        invalidRequests.append(Data([0x84, 0x02]) + fields + maximum)
        invalidRequests.append(Data([0x85, 0x01]) + fields + maximum + Data([0]))
        invalidRequests.append(request.encoded + Data([0]))
        invalidRequests.append(Data(request.encoded.dropLast()))
        invalidRequests.append(Data([0x9f, 0x01]) + fields + maximum + Data([0xff]))
        for data in invalidRequests {
            XCTAssertThrowsError(try RecipientRequest(encoded: data))
        }
        for badSize in [0, 4221, FTLimit.envelope + 1, -1] {
            XCTAssertThrowsError(try RecipientRequest(requestID: request.requestID, publicKey: request.publicKey, maximumEnvelopeBytes: badSize))
        }
        XCTAssertThrowsError(try RecipientRequest(requestID: Data(repeating: 0, count: 15), publicKey: request.publicKey, maximumEnvelopeBytes: FTLimit.envelope))
        XCTAssertThrowsError(try RecipientRequest(requestID: request.requestID, publicKey: Data(repeating: 0, count: 31), maximumEnvelopeBytes: FTLimit.envelope))
        // Neither an unwrapped tuple nor two CBOR messages are a valid ur:bytes request.
        XCTAssertThrowsError(try RecipientRequest(text: "FTREQ1:UR:BYTES/" + Bytewords.encode(request.encoded)))
        XCTAssertThrowsError(try RecipientRequest(text: "FTREQ1:UR:BYTES/" + Bytewords.encode(CBORWriter.bytes(request.encoded) + Data([0]))))
    }

    func testModeTwoRoundTripAndEverySingleByteMutationIsRejected() throws {
        let session = try RecipientSession(using: crypto), archive = Data("recipient transfer test archive".utf8)
        let envelope = try FTAG.sealArchiveBytes(archive, recipient: session.request, transferID: Data(repeating: 17, count: 16), using: crypto)
        let header = try FTAGHeader(envelope: envelope)
        XCTAssertEqual(header.mode, .recipient); XCTAssertEqual(header.header.count, 110)
        XCTAssertEqual(header.modeHeader.count, 80); XCTAssertEqual(envelope.count, 4222)
        XCTAssertEqual(try FTAG.openArchiveBytes(envelope, recipient: session, using: crypto), archive)
        for offset in envelope.indices {
            var changed = envelope; changed[offset] ^= 1
            XCTAssertThrowsError(try FTAG.openArchiveBytes(changed, recipient: session, using: crypto), "accepted corrupted byte \(offset)")
        }
        XCTAssertThrowsError(try FTAG.openArchiveBytes(envelope + Data([0]), recipient: session, using: crypto))
        XCTAssertThrowsError(try FTAG.openArchiveBytes(Data(envelope.dropLast()), recipient: session, using: crypto))
    }

    func testRecipientBindingAndBothModesRefuseFallback() throws {
        let session = try RecipientSession(using: crypto), other = try RecipientSession(using: crypto)
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        let recipientEnvelope = try FTAG.seal(archive, recipient: session.request, using: crypto)
        let codeExport = try FTAG.seal(archive, using: crypto)
        XCTAssertThrowsError(try FTAG.open(recipientEnvelope, recipient: other, using: crypto))
        XCTAssertThrowsError(try FTAG.open(recipientEnvelope, code: codeExport.code, using: crypto))
        XCTAssertThrowsError(try FTAG.open(codeExport.envelope, recipient: session, using: crypto))
        let reopened = try FTAG.open(recipientEnvelope, recipient: session, using: crypto)
        XCTAssertEqual(reopened.workspace.raw, archive.workspace.raw)
        XCTAssertFalse(String(describing: session).contains(session.request.publicKey.ftHex))
        XCTAssertEqual(String(reflecting: session), "RecipientSession([private key redacted])")
    }

    func testPublicRequestAllowsAnotherSenderButNeverRevealsRecipientKey() throws {
        let session = try RecipientSession(using: crypto)
        // Base mode deliberately permits anyone with this public request to seal.
        // Successful decryption cannot be treated as source authentication.
        let observedRequest = try RecipientRequest(text: session.request.text)
        let foreign = try FTAG.sealArchiveBytes(Data("a third-party archive".utf8), recipient: observedRequest,
                                               transferID: Data(repeating: 5, count: 16), using: crypto)
        XCTAssertEqual(try FTAG.openArchiveBytes(foreign, recipient: session, using: crypto), Data("a third-party archive".utf8))
        XCTAssertThrowsError(try FTAG.openArchiveBytes(foreign, recipient: RecipientSession(using: crypto), using: crypto))
    }

    func testAdvertisedEnvelopeLimitIncludesModeTwoHeaderAndTag() throws {
        let session = try RecipientSession(maximumEnvelopeBytes: 4222, using: crypto)
        let transferID = Data(repeating: 9, count: 16)
        let exact = try FTAG.sealArchiveBytes(Data(repeating: 11, count: 4087), recipient: session.request,
                                             transferID: transferID, compress: false, using: crypto)
        XCTAssertEqual(exact.count, 4222)
        XCTAssertEqual(try FTAG.openArchiveBytes(exact, recipient: session, using: crypto).count, 4087)
        XCTAssertThrowsError(try FTAG.sealArchiveBytes(Data(repeating: 11, count: 4088), recipient: session.request,
                                                      transferID: transferID, compress: false, using: crypto))
        XCTAssertThrowsError(try FTAG.pack(Data(repeating: 7, count: FTLimit.envelope - 9), compress: false, mode: .recipient))
        // The last permitted padding bucket still fits, with the complete mode2 overhead.
        let largest = try FTAG.pack(Data(repeating: 7, count: FTLimit.envelope - 4096 - 9), compress: false, mode: .recipient)
        XCTAssertLessThanOrEqual(largest.count + 126, FTLimit.envelope)
    }

    func testRecipientCiphertextWorksThroughExistingFountainTransport() throws {
        let session = try RecipientSession(using: crypto)
        let payload = Data((0..<70000).map { UInt8(truncatingIfNeeded: $0) })
        let envelope = try FTAG.sealArchiveBytes(payload, recipient: session.request, transferID: Data(repeating: 12, count: 16),
                                                compress: false, using: crypto)
        let sender = try OpticalSender(envelope: envelope, using: crypto)
        var receiver = OpticalReceiver(selecting: try OpticalFrame(text: sender.frame(page: 1, sequence: 2, using: crypto)))
        for page in (0..<sender.identity.pageCount).reversed() {
            for sequence in (1...(try sender.sourcePartCount(page: page))).reversed() {
                try receiver.receive(OpticalFrame(text: sender.frame(page: page, sequence: UInt32(sequence), using: crypto)), using: crypto)
            }
        }
        XCTAssertEqual(try receiver.envelope(using: crypto), envelope)
        XCTAssertEqual(try FTAG.openArchiveBytes(receiver.envelope(using: crypto), recipient: session, using: crypto), payload)
    }
}

@MainActor
final class RecipientRestoreTests: XCTestCase {
    let crypto = TestCryptography()

    func environment() -> (OfflineRestore<TestCryptography>, WorkspaceStore, StoreTestPersistence, StoreTestVault) {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        return (OfflineRestore(using: crypto, store: store), store, persistence, vault)
    }

    func archive() throws -> AirGapArchive {
        let (key, fingerprint) = try sampleKey(using: crypto)
        return try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint)), keys: [key], using: crypto)
    }

    func testRecipientRestoreStillRequiresSourceReviewBeforeAnyStorage() async throws {
        let (restore, store, persistence, vault) = environment()
        var state = await restore.status()
        state = try await restore.createRecipientRequest(generation: state.generation)
        let request = try XCTUnwrap(state.recipientRequest)
        let envelope = try FTAG.seal(archive(), recipient: RecipientRequest(text: request.text), using: crypto)
        state = try await restore.acceptEncryptedFile(envelope, generation: state.generation)
        XCTAssertEqual(state.encryptionMode, .recipient); XCTAssertTrue(state.canUnlockRecipient)
        state = try await restore.unlockRecipient(generation: state.generation)
        XCTAssertEqual(state.phase, .review); XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.catalog))
        let review = try XCTUnwrap(state.review)
        do {
            _ = try await restore.commit(reviewToken: review.token, generation: state.generation, sourceAndContentsConfirmed: false)
            XCTFail("HPKE authentication bypassed source review")
        } catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.journal))
        state = try await restore.commit(reviewToken: review.token, generation: state.generation, sourceAndContentsConfirmed: true)
        XCTAssertEqual(state.phase, .complete); XCTAssertNil(state.recipientRequest); XCTAssertFalse(state.canUnlockRecipient)
        XCTAssertEqual(vault.count, 1)
        let workspaces = try await store.load()
        XCTAssertEqual(workspaces.count, 1); XCTAssertTrue(workspaces[0].connectLater)
    }

    func testSuspendedRequestNeedsExplicitResumeAndFreshReview() async throws {
        let (restore, _, persistence, vault) = environment()
        var state = await restore.status()
        state = try await restore.createRecipientRequest(generation: state.generation)
        let request = try XCTUnwrap(state.recipientRequest)
        let envelope = try FTAG.seal(archive(), recipient: request, using: crypto)
        state = try await restore.acceptEncryptedFile(envelope, generation: state.generation)
        state = try await restore.unlockRecipient(generation: state.generation)
        let prior = state
        state = await restore.suspend()
        XCTAssertTrue(state.requiresRecipientResume); XCTAssertNil(state.review); XCTAssertFalse(state.canUnlockRecipient)
        state = await restore.resume()
        XCTAssertTrue(state.isSuspended); XCTAssertEqual(state.recipientRequest, request)
        do { _ = try await restore.unlockRecipient(generation: state.generation); XCTFail("automatic recipient resume") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        state = try await restore.resumeRecipientRequest(generation: state.generation)
        XCTAssertFalse(state.isSuspended); XCTAssertEqual(state.phase, .locked); XCTAssertTrue(state.canUnlockRecipient)
        do { _ = try await restore.unlockRecipient(generation: prior.generation); XCTFail("retired callback") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        state = try await restore.unlockRecipient(generation: state.generation)
        XCTAssertNotEqual(state.review?.token, prior.review?.token)
        XCTAssertEqual(vault.creates, 0); XCTAssertNil(persistence.bytes(.catalog))
    }

    func testCancellationDropsRecipientKeyAndCannotFallBackToCode() async throws {
        let (restore, _, persistence, vault) = environment()
        var state = await restore.status()
        state = try await restore.createRecipientRequest(generation: state.generation)
        let request = try XCTUnwrap(state.recipientRequest)
        let envelope = try FTAG.seal(archive(), recipient: request, using: crypto)
        state = try await restore.acceptEncryptedFile(envelope, generation: state.generation)
        _ = await restore.suspend(discardRecipientRequest: true)
        state = await restore.resume()
        XCTAssertNil(state.recipientRequest); XCTAssertFalse(state.canUnlockRecipient)
        do { _ = try await restore.unlockRecipient(generation: state.generation); XCTFail("cancelled key retained") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        do { _ = try await restore.unlock(enteredCode: "anything", generation: state.generation); XCTFail("recipient mode fallback") }
        catch { XCTAssertEqual(error as? FTError, .unsupported("envelope.requires_recipient_key")) }
        state = try await restore.reset()
        state = try await restore.createRecipientRequest(generation: state.generation)
        XCTAssertNotEqual(state.recipientRequest, request)
        state = try await restore.acceptEncryptedFile(envelope, generation: state.generation)
        XCTAssertFalse(state.canUnlockRecipient)
        do { _ = try await restore.unlockRecipient(generation: state.generation); XCTFail("new request unlocked old recording") }
        catch { XCTAssertEqual(error as? FTError, .authentication) }
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.creates, 0)
    }

    func testRequestCannotBeReplacedOrDowngradedByAnIncomingPackage() async throws {
        let (restore, _, persistence, vault) = environment()
        var state = await restore.status()
        state = try await restore.createRecipientRequest(generation: state.generation)
        let request = state.recipientRequest
        do { _ = try await restore.createRecipientRequest(generation: state.generation); XCTFail("replaced active request") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .busy) }
        do { _ = try await restore.acceptWorkspaceDocument(sampleManifest(), generation: state.generation); XCTFail("public document replaced request") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .busy) }
        let codeExport = try FTAG.seal(archive(), using: crypto)
        state = try await restore.acceptEncryptedFile(codeExport.envelope, generation: state.generation)
        XCTAssertEqual(state.recipientRequest, request)
        do { _ = try await restore.unlock(enteredCode: codeExport.code.reveal(using: crypto), generation: state.generation); XCTFail("recipient mode silently downgraded") }
        catch { XCTAssertEqual(error as? FTError, .unsupported("envelope.requires_recipient_key")) }
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.creates, 0)
    }
}
