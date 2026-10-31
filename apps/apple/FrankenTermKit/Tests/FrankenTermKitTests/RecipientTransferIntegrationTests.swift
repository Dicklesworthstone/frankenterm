import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class RecipientTransferIntegrationTests: XCTestCase {
    let crypto = TestCryptography()

    func testCanonicalRecipientRequestMatchesIndependentBytesAndRoundTripsText() async throws {
        let vector = try RecipientHPKEFixtures.load().envelopes[0]
        let request = try RecipientRequest(encoded: Data(ftHex: vector.requestCBOR))
        XCTAssertEqual(request.requestID, Data(0..<16))
        XCTAssertEqual(request.publicKey, try Data(ftHex: vector.publicKey))
        XCTAssertEqual(request.maximumEnvelopeBytes, FTLimit.envelope)
        XCTAssertEqual(request.digest(using: crypto), try Data(ftHex: vector.requestDigest))
        XCTAssertEqual(request.comparison(using: crypto), FTAG.comparison(request.encoded, using: crypto))
        XCTAssertEqual(request.encoded.count, 58)
        XCTAssertEqual(request.text.utf8.count, 144)
        XCTAssertEqual(try RecipientRequest(text: request.text), request)
        XCTAssertThrowsError(try RecipientRequest(text: request.text.lowercased()))
        XCTAssertThrowsError(try OpticalFrame(text: request.text))
    }

    func testRecipientRequestRejectsNoncanonicalAmbiguousAndUnboundedInputs() async throws {
        let vector = try RecipientHPKEFixtures.load().envelopes[0]
        let request = try RecipientRequest(encoded: Data(ftHex: vector.requestCBOR))
        let prefix = CBORWriter.array(4) + CBORWriter.uint(1)
        var invalid: [Data] = [Data(), request.encoded + Data([0]), Data(repeating: 0, count: 4097)]
        invalid.append(Data([0x84, 0x18, 0x01]) + request.encoded.dropFirst(2))
        invalid.append(CBORWriter.array(4) + CBORWriter.uint(2) + request.encoded.dropFirst(2))
        for count in [0, 15, 17] {
            invalid.append(prefix + CBORWriter.bytes(Data(repeating: 0, count: count)) + CBORWriter.bytes(request.publicKey)
                + CBORWriter.uint(UInt64(FTLimit.envelope)))
        }
        for count in [0, 31, 33] {
            invalid.append(prefix + CBORWriter.bytes(request.requestID) + CBORWriter.bytes(Data(repeating: 0, count: count))
                + CBORWriter.uint(UInt64(FTLimit.envelope)))
        }
        for maximum in [0, 4222 - 1, FTLimit.envelope + 1] {
            invalid.append(prefix + CBORWriter.bytes(request.requestID) + CBORWriter.bytes(request.publicKey) + CBORWriter.uint(UInt64(maximum)))
        }
        for encoded in invalid { XCTAssertThrowsError(try RecipientRequest(encoded: encoded)) }
        let badChecksum = String(request.text.dropLast(2)) + (request.text.hasSuffix("AE") ? "AD" : "AE")
        for text in ["", "https://example.invalid/" + request.text, " " + request.text, request.text + "\n", badChecksum,
                     request.text + "/", request.text + "é", "FTREQ1:UR:BYTES/1-2/AAAA", String(repeating: "A", count: 4097)] {
            XCTAssertThrowsError(try RecipientRequest(text: text))
        }
        XCTAssertEqual(try RecipientRequest(requestID: request.requestID, publicKey: request.publicKey,
            maximumEnvelopeBytes: 4222).maximumEnvelopeBytes, 4222)
    }

    func testIndependentPythonRawAndCompressedEnvelopesOpenWithoutAnyGeneratedCode() async throws {
        for vector in try RecipientHPKEFixtures.load().envelopes {
            let request = try RecipientRequest(encoded: Data(ftHex: vector.requestCBOR))
            let envelope = try Data(ftHex: vector.envelope), privateKey = try Data(ftHex: vector.privateKey)
            XCTAssertEqual(crypto.sha256(envelope), try Data(ftHex: vector.envelopeSHA256))
            let header = try FTAGHeader(envelope: envelope)
            XCTAssertEqual(header.mode, .recipient)
            XCTAssertEqual(header.header.count, 110)
            XCTAssertEqual(header.modeHeader.count, 80)
            XCTAssertEqual(Data(header.modeHeader.prefix(16)), request.requestID)
            XCTAssertEqual(Data(header.modeHeader.dropFirst(16).prefix(32)), request.digest(using: crypto))
            XCTAssertEqual(header.modeHeader.suffix(32).count, 32)
            XCTAssertEqual(try FTAG.openArchiveBytes(envelope, recipient: session(for: request, privateKey: privateKey), using: crypto),
                           try Data(ftHex: vector.archive), vector.id)
            let archive = try FTAG.open(envelope, recipient: session(for: request, privateKey: privateKey), using: crypto)
            XCTAssertEqual(archive.workspace.raw, try WorkspaceDocument(data: Data(vector.manifest.utf8)).raw, vector.id)
        }
    }

    func testEveryRecipientHeaderByteAndCiphertextExtentIsBoundToAuthentication() async throws {
        let vector = try RecipientHPKEFixtures.load().envelopes[1]
        let request = try RecipientRequest(encoded: Data(ftHex: vector.requestCBOR))
        let original = try Data(ftHex: vector.envelope), privateKey = try Data(ftHex: vector.privateKey)
        for position in 0..<110 {
            var changed = original; changed[position] ^= 1
            XCTAssertThrowsError(try FTAG.open(changed, recipient: session(for: request, privateKey: privateKey), using: crypto), "header byte \(position)")
        }
        for position in [110, original.count / 2, original.count - 1] {
            var changed = original; changed[position] ^= 1
            XCTAssertThrowsError(try FTAG.open(changed, recipient: session(for: request, privateKey: privateKey), using: crypto)) {
                XCTAssertEqual($0 as? FTError, .authentication)
            }
        }
        for changed in [Data(original.dropLast()), original + Data([0])] {
            XCTAssertThrowsError(try FTAG.open(changed, recipient: session(for: request, privateKey: privateKey), using: crypto))
        }
    }

    func testExactRequestConfirmationPrivateKeyAndModeCannotBeSubstituted() async throws {
        let vector = try RecipientHPKEFixtures.load().envelopes[1]
        let request = try RecipientRequest(encoded: Data(ftHex: vector.requestCBOR))
        let envelope = try Data(ftHex: vector.envelope), privateKey = try Data(ftHex: vector.privateKey)
        let document = try archive()
        var wrongConfirmation = request.digest(using: crypto); wrongConfirmation[31] ^= 1
        XCTAssertEqual(wrongConfirmation.prefix(10), request.digest(using: crypto).prefix(10), "Display comparison alone is not the approval token.")
        for confirmation in [Data(), Data(request.digest(using: crypto).prefix(10)), wrongConfirmation] {
            XCTAssertThrowsError(try FTAG.seal(document, recipient: request, confirmedRequestDigest: confirmation, using: crypto)) {
                XCTAssertEqual($0 as? FTError, .unsupported("recipient.confirmation_required"))
            }
        }
        let changedRequest = try RecipientRequest(requestID: Data(repeating: 0xA5, count: 16), publicKey: request.publicKey,
                                                   maximumEnvelopeBytes: request.maximumEnvelopeBytes)
        XCTAssertThrowsError(try FTAG.open(envelope, recipient: session(for: changedRequest, privateKey: privateKey), using: crypto)) {
            XCTAssertEqual($0 as? FTError, .authentication)
        }
        XCTAssertThrowsError(try session(for: request, privateKey: Data())) {
            XCTAssertEqual($0 as? FTError, .malformed("recipient.private_key"))
        }
        var wrongKey = privateKey; wrongKey[16] ^= 1
        XCTAssertThrowsError(try FTAG.open(envelope, recipient: session(for: request, privateKey: wrongKey), using: crypto)) {
            XCTAssertEqual($0 as? FTError, .authentication)
        }
        let codeExport = try FTAG.seal(document, using: crypto)
        XCTAssertThrowsError(try FTAG.open(codeExport.envelope, recipient: session(for: request, privateKey: privateKey), using: crypto)) {
            XCTAssertEqual($0 as? FTError, .unsupported("envelope.requires_transfer_code"))
        }
        XCTAssertThrowsError(try FTAG.open(envelope, code: codeExport.code, using: crypto)) {
            XCTAssertEqual($0 as? FTError, .unsupported("envelope.requires_recipient_key"))
        }
    }

    func testAdvertisedRecipientCapAppliesBeforeSealingAndBeforeDecoderAllocation() async throws {
        let (restore, _, persistence, vault) = environment()
        let initial = await restore.status()
        let waiting = try await restore.createRecipientRequest(generation: initial.generation,
            maximumEnvelopeBytes: 4222)
        let request = try XCTUnwrap(waiting.recipientRequest)
        XCTAssertThrowsError(try FTAG.sealArchiveBytes(Data(repeating: 1, count: 8000), recipient: request,
            transferID: Data(repeating: 2, count: 16), compress: false, using: crypto))
        let vector = try RecipientHPKEFixtures.load().envelopes[0]
        let sender = try OpticalSender(envelope: Data(ftHex: vector.envelope), using: crypto)
        do { _ = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: waiting.generation); XCTFail("oversized candidate accepted") }
        catch { XCTAssertEqual(error as? FTError, .limit("recipient.envelope_limit")) }
        let after = await restore.status()
        XCTAssertEqual(after.phase, .idle)
        XCTAssertNil(after.identity)
        XCTAssertEqual(after.independentParts, 0)
        XCTAssertEqual(after.recipientRequest, request)
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.creates, 0)
    }

    func testRecipientOpticalTransferRequiresSourceReviewBeforePrivateKeyPublication() async throws {
        let (restore, store, persistence, vault) = environment()
        let initial = await restore.status()
        var state = try await restore.createRecipientRequest(generation: initial.generation)
        XCTAssertNotEqual(state.generation, initial.generation)
        let request = try XCTUnwrap(state.recipientRequest), exported = try export(for: request)
        let sender = try OpticalSender(envelope: exported, using: crypto)
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        XCTAssertEqual(state.phase, .candidate)
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        for page in 0..<sender.identity.pageCount {
            for sequence in 1...(try sender.sourcePartCount(page: page)) {
                state = try await restore.observe(sender.frame(page: page, sequence: UInt32(sequence), using: crypto), generation: state.generation)
            }
        }
        XCTAssertEqual(state.phase, .locked)
        XCTAssertEqual(state.encryptionMode, .recipient)
        XCTAssertEqual(state.verifiedBytes, exported.count)
        do { _ = try await restore.unlock(enteredCode: "not a transfer code", generation: state.generation); XCTFail("recipient mode used code path") }
        catch { XCTAssertEqual(error as? FTError, .unsupported("envelope.requires_recipient_key")) }
        state = try await restore.unlockRecipient(generation: state.generation)
        XCTAssertEqual(state.phase, .review)
        let review = try XCTUnwrap(state.review)
        XCTAssertEqual(review.includedFingerprints.count, 1)
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.creates, 0)
        do { _ = try await restore.commit(reviewToken: review.token, generation: state.generation, sourceAndContentsConfirmed: false); XCTFail("unreviewed sender imported") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .reviewRequired) }
        XCTAssertEqual(vault.creates, 0)
        state = try await restore.commit(reviewToken: review.token, generation: state.generation, sourceAndContentsConfirmed: true)
        XCTAssertEqual(state.phase, .complete)
        XCTAssertNil(state.recipientRequest)
        XCTAssertEqual(vault.count, 1)
        XCTAssertTrue(try XCTUnwrap(state.receipt).workspace.connectLater)
        let saved = try await store.load()
        XCTAssertEqual(saved.count, 1)
    }

    func testActiveRecipientRejectsModeDowngradeAndWrongRequestWithoutLosingItsKey() async throws {
        let (restore, _, persistence, vault) = environment()
        let initial = await restore.status(), codeExport = try FTAG.seal(archive(), using: crypto)
        let waiting = try await restore.createRecipientRequest(generation: initial.generation)
        let request = try XCTUnwrap(waiting.recipientRequest)
        do { _ = try await restore.acceptEncryptedFile(codeExport.envelope, generation: waiting.generation); XCTFail("mode downgraded") }
        catch { XCTAssertEqual(error as? FTError, .unsupported("envelope.requires_recipient_key")) }
        do { _ = try await restore.acceptWorkspaceDocument(sampleManifest(), generation: waiting.generation); XCTFail("public file replaced recipient") }
        catch { XCTAssertEqual(error as? WorkspaceStoreError, .busy) }
        let unrelated = try RecipientHPKEFixtures.load().envelopes[1]
        do { _ = try await restore.acceptEncryptedFile(Data(ftHex: unrelated.envelope), generation: waiting.generation); XCTFail("wrong recipient accepted") }
        catch { XCTAssertEqual(error as? FTError, .authentication) }
        let after = await restore.status()
        XCTAssertEqual(after.phase, .idle); XCTAssertEqual(after.recipientRequest, request)
        XCTAssertEqual(after.generation, waiting.generation)
        let valid = try export(for: request)
        let locked = try await restore.acceptEncryptedFile(valid, generation: after.generation)
        let reviewed = try await restore.unlockRecipient(generation: locked.generation)
        XCTAssertEqual(reviewed.phase, .review)
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.creates, 0)
    }

    func testWrongOpticalDestinationRetiresOnlyItsSourceAndFencesLateFrames() async throws {
        let (restore, _, _, vault) = environment()
        let initial = await restore.status()
        var state = try await restore.createRecipientRequest(generation: initial.generation)
        let request = try XCTUnwrap(state.recipientRequest)
        let unrelated = try RecipientHPKEFixtures.load().envelopes[1]
        let sender = try OpticalSender(envelope: Data(ftHex: unrelated.envelope), using: crypto)
        state = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: state.generation)
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        let stale = state.generation
        var rejected = false
        for sequence in 1...(try sender.sourcePartCount(page: 0)) {
            do { state = try await restore.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto), generation: stale) }
            catch { XCTAssertEqual(error as? FTError, .authentication); rejected = true; break }
        }
        XCTAssertTrue(rejected)
        state = await restore.status()
        XCTAssertEqual(state.phase, .idle); XCTAssertEqual(state.recipientRequest, request)
        XCTAssertNil(state.identity); XCTAssertEqual(state.independentParts, 0); XCTAssertNotEqual(state.generation, stale)
        do { _ = try await restore.observe(sender.frame(page: 0, sequence: 1, using: crypto), generation: stale); XCTFail("late source callback accepted") }
        catch { XCTAssertEqual(error as? FTError, .stale) }
        let valid = try export(for: request)
        _ = try await restore.acceptEncryptedFile(valid, generation: state.generation)
        let reviewed = try await restore.unlockRecipient(generation: state.generation)
        XCTAssertEqual(reviewed.phase, .review); XCTAssertEqual(vault.creates, 0)
    }

    func testMalformedFirstSymbolRetainsThePhoneRequestAndAcceptsTheNextHealthyResponse() async throws {
        let (restore, _, persistence, vault) = environment()
        let initial = await restore.status()
        var state = try await restore.createRecipientRequest(generation: initial.generation)
        let request = try XCTUnwrap(state.recipientRequest), envelope = try export(for: request)
        let sender = try OpticalSender(envelope: envelope, using: crypto)
        let first = try sender.frame(page: 0, sequence: 1, using: crypto)
        let prefix = first.split(separator: "/", maxSplits: 6).prefix(6).joined(separator: "/") + "/"
        state = try await restore.observe(prefix + "UR:BYTES/AAAA", generation: state.generation)
        let rejectedGeneration = state.generation
        do {
            _ = try await restore.selectCandidate(generation: rejectedGeneration, digest: sender.identity.envelopeDigest)
            XCTFail("selected a malformed recipient response")
        } catch { XCTAssertEqual(error as? FTError, .malformed("bytewords.extent")) }
        state = await restore.status()
        XCTAssertEqual(state.phase, .idle); XCTAssertNil(state.identity)
        XCTAssertNotEqual(state.generation, rejectedGeneration)
        XCTAssertEqual(state.recipientRequest, request, "Only the bad source is discarded, not the phone's private request.")
        XCTAssertEqual(state.receivedParts, 0); XCTAssertNil(state.review)
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.creates, 0)
        do {
            _ = try await restore.observe(first, generation: rejectedGeneration)
            XCTFail("a queued response entered the retired selection generation")
        } catch { XCTAssertEqual(error as? FTError, .stale) }
        state = try await restore.observe(first, generation: state.generation)
        state = try await restore.selectCandidate(generation: state.generation, digest: sender.identity.envelopeDigest)
        for sequence in 2...(try sender.sourcePartCount(page: 0)) {
            state = try await restore.observe(sender.frame(page: 0, sequence: UInt32(sequence), using: crypto),
                                              generation: state.generation)
        }
        XCTAssertEqual(state.phase, .locked); XCTAssertTrue(state.canUnlockRecipient)
        state = try await restore.unlockRecipient(generation: state.generation)
        XCTAssertEqual(state.phase, .review)
        XCTAssertEqual(state.review?.includedFingerprints.count, 1)
        XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.creates, 0)
    }

    func testTemporarySuspensionRequiresManualResumeAndCancellationLosesTheKey() async throws {
        for suspendAfterReview in [false, true] {
            let (restore, store, persistence, vault) = environment()
            let initial = await restore.status()
            var state = try await restore.createRecipientRequest(generation: initial.generation)
            let request = try XCTUnwrap(state.recipientRequest), exported = try export(for: request)
            state = try await restore.acceptEncryptedFile(exported, generation: state.generation)
            if suspendAfterReview { state = try await restore.unlockRecipient(generation: state.generation) }
            let staleGeneration = state.generation
            let backgrounded = await restore.suspend()
            XCTAssertTrue(backgrounded.requiresRecipientResume); XCTAssertEqual(backgrounded.recipientRequest, request)
            XCTAssertNil(backgrounded.review); XCTAssertFalse(backgrounded.canUnlockRecipient)
            let automatic = await restore.resume()
            XCTAssertTrue(automatic.isSuspended)
            let resumed = try await restore.resumeRecipientRequest(generation: automatic.generation)
            XCTAssertEqual(resumed.phase, .locked); XCTAssertTrue(resumed.canUnlockRecipient)
            XCTAssertNotEqual(resumed.generation, staleGeneration)
            let reviewedAgain = try await restore.unlockRecipient(generation: resumed.generation)
            XCTAssertEqual(reviewedAgain.phase, .review)
            if let oldReview = state.review { XCTAssertNotEqual(reviewedAgain.review?.token, oldReview.token) }
            let cancelled = await restore.suspend(discardRecipientRequest: true)
            XCTAssertNil(cancelled.recipientRequest); XCTAssertFalse(cancelled.canUnlockRecipient)
            XCTAssertNil(cancelled.review)
            let afterCancellation = await restore.resume()
            XCTAssertFalse(afterCancellation.isSuspended)
            XCTAssertEqual(afterCancellation.phase, .locked)
            do { _ = try await restore.unlockRecipient(generation: staleGeneration); XCTFail("stale session resurrected") }
            catch { XCTAssertEqual(error as? FTError, .stale) }
            let newSession = OfflineRestore(using: crypto, store: store)
            let newState = await newSession.status()
            let orphan = try await newSession.acceptEncryptedFile(exported, generation: newState.generation)
            do { _ = try await newSession.unlockRecipient(generation: orphan.generation); XCTFail("lost private request reconstructed") }
            catch { XCTAssertEqual(error as? FTError, .stale) }
            do { _ = try await newSession.unlock(enteredCode: "anything", generation: orphan.generation); XCTFail("fallback accepted") }
            catch { XCTAssertEqual(error as? FTError, .unsupported("envelope.requires_recipient_key")) }
            let reset = try await restore.reset()
            let fresh = try await restore.createRecipientRequest(generation: reset.generation)
            XCTAssertNotEqual(fresh.recipientRequest?.requestID, request.requestID)
            XCTAssertNotEqual(fresh.recipientRequest?.publicKey, request.publicKey)
            do { _ = try await restore.acceptEncryptedFile(exported, generation: fresh.generation); XCTFail("old recording accepted for new request") }
            catch { XCTAssertEqual(error as? FTError, .authentication) }
            XCTAssertNil(persistence.bytes(.catalog)); XCTAssertEqual(vault.creates, 0)
        }
    }

    func testAuthenticatedMalformedArchiveAndTamperedTagCannotReachStorage() async throws {
        for malformedArchive in [false, true] {
            let (restore, _, persistence, vault) = environment()
            let initial = await restore.status()
            let waiting = try await restore.createRecipientRequest(generation: initial.generation)
            let request = try XCTUnwrap(waiting.recipientRequest)
            var envelope: Data
            if malformedArchive {
                envelope = try FTAG.sealArchiveBytes(Data([0x80]), recipient: request,
                    transferID: Data(repeating: 0x7F, count: 16), using: crypto)
            } else {
                envelope = try export(for: request)
                envelope[envelope.count - 1] ^= 1
            }
            let locked = try await restore.acceptEncryptedFile(envelope, generation: waiting.generation)
            do { _ = try await restore.unlockRecipient(generation: locked.generation); XCTFail("invalid archive accepted") }
            catch { XCTAssertNotNil(error as? FTError) }
            let after = await restore.status()
            XCTAssertEqual(after.phase, .locked); XCTAssertNil(after.review)
            XCTAssertNil(persistence.bytes(.catalog)); XCTAssertNil(persistence.bytes(.journal)); XCTAssertEqual(vault.creates, 0)
        }
    }

    func testRecipientPublicationRecoverySurvivesDestructionOfTheTemporaryPrivateKey() async throws {
        for published in [false, true] {
            let (restore, _, persistence, vault) = environment()
            let initial = await restore.status()
            var state = try await restore.createRecipientRequest(generation: initial.generation)
            let request = try XCTUnwrap(state.recipientRequest), exported = try export(for: request)
            state = try await restore.acceptEncryptedFile(exported, generation: state.generation)
            state = try await restore.unlockRecipient(generation: state.generation)
            let review = try XCTUnwrap(state.review)
            persistence.configure(published ? .after(.catalog) : .before(.catalog))
            do { _ = try await restore.commit(reviewToken: review.token, generation: state.generation, sourceAndContentsConfirmed: true); XCTFail("fault did not fire") }
            catch { XCTAssertEqual(error as? FTError, .storage) }
            let failed = await restore.status()
            XCTAssertEqual(failed.phase, .recoveryRequired)
            _ = await restore.suspend(discardRecipientRequest: true)
            let resumed = await restore.resume()
            XCTAssertNil(resumed.recipientRequest); XCTAssertFalse(resumed.canUnlockRecipient)
            let reconciled = try await restore.reconcile(generation: resumed.generation)
            XCTAssertEqual(reconciled.phase, published ? .complete : .locked)
            XCTAssertEqual(vault.count, published ? 1 : 0)
            if published { XCTAssertNotNil(reconciled.receipt) }
            else {
                do { _ = try await restore.unlockRecipient(generation: reconciled.generation); XCTFail("cancelled key survived rollback") }
                catch { XCTAssertEqual(error as? FTError, .stale) }
                let reset = try await restore.reset()
                XCTAssertEqual(reset.phase, .idle); XCTAssertNil(reset.identity); XCTAssertNil(reset.encryptionMode)
            }
        }
    }

    func testSwiftRecipientEnvelopeEmitsCrossPlatformOracleInput() async throws {
        let vector = try RecipientHPKEFixtures.load().envelopes[0]
        let request = try RecipientRequest(encoded: Data(ftHex: vector.requestCBOR)), archive = try archive()
        let exported = try FTAG.seal(archive, recipient: request, confirmedRequestDigest: request.digest(using: crypto), using: crypto)
        let opened = try FTAG.open(exported, recipient: session(for: request, privateKey: Data(ftHex: vector.privateKey)), using: crypto)
        XCTAssertEqual(opened.workspace.raw, archive.workspace.raw)
        if let path = ProcessInfo.processInfo.environment["FT_HPKE_VALIDATION_DIRECTORY"] {
            let output = URL(fileURLWithPath: path, isDirectory: true)
            try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
            let values: [String: String] = ["privateKey": vector.privateKey, "publicKey": vector.publicKey,
                "requestCBOR": request.encoded.ftHex, "requestDigest": request.digest(using: crypto).ftHex,
                "envelope": exported.ftHex, "archive": try archive.encoded().ftHex]
            try JSONSerialization.data(withJSONObject: values, options: [.sortedKeys, .prettyPrinted])
                .write(to: output.appendingPathComponent("swift-recipient-envelope.json"), options: .withoutOverwriting)
        }
    }

    private func archive() throws -> AirGapArchive {
        let (key, fingerprint) = try sampleKey(using: crypto)
        return try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint)), keys: [key], using: crypto)
    }
    private func export(for request: RecipientRequest) throws -> Data {
        try FTAG.seal(archive(), recipient: request, confirmedRequestDigest: request.digest(using: crypto), using: crypto)
    }
    private func session(for request: RecipientRequest, privateKey: Data) throws -> RecipientSession {
        try RecipientSession(privateKey: privateKey, requestID: request.requestID,
                             maximumEnvelopeBytes: request.maximumEnvelopeBytes, using: crypto)
    }
    private func environment() -> (OfflineRestore<TestCryptography>, WorkspaceStore, StoreTestPersistence, StoreTestVault) {
        let persistence = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: persistence, vault: vault)
        return (OfflineRestore(using: crypto, store: store), store, persistence, vault)
    }
}
