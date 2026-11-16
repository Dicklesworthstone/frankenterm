import Foundation
import XCTest
@testable import FrankenTermKit

final class WorkspaceKeyEnrollmentTests: XCTestCase {
    private func key(id: String = "phone-key-00000000-0000-4000-8000-000000000001") throws -> WorkspaceDeviceKey {
        // RFC 8032 test vector 1's public point, wrapped in the SSH wire format.
        let point = Data([
            0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7,
            0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
            0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25,
            0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a
        ])
        let blob = SSHUserAuthenticationChallenge.string(Data("ssh-ed25519".utf8)) +
            SSHUserAuthenticationChallenge.string(point)
        return WorkspaceDeviceKey(id: id, fingerprint: try SSHPublicIdentity(blob: blob).fingerprint, publicBlob: blob)
    }

    private func request(_ uuid: String = "10000000-0000-4000-8000-000000000001") throws -> WorkspaceKeyEnrollmentRequest {
        try .init(key: key(), requestID: XCTUnwrap(UUID(uuidString: uuid)))
    }

    private func target(_ host: String = "agent.example", port: UInt16 = 22,
                        user: String = "operator") throws -> WorkspaceKeyEnrollmentTarget {
        try .init(host: host, port: port, user: user)
    }

    private func edited(_ data: Data, change: (inout [String: JSONValue]) throws -> Void) throws -> Data {
        var root = try StrictJSON.decode(data).object()
        try change(&root)
        return try StrictJSON.encode(.object(root))
    }

    private func result(_ request: WorkspaceKeyEnrollmentRequest, _ target: WorkspaceKeyEnrollmentTarget,
                        _ state: WorkspaceKeyEnrollmentState) throws -> WorkspaceKeyEnrollmentResult {
        try .init(operationID: request.operationID(for: target), target: target, state: state,
                  detail: "Locally generated status.")
    }

    func testPublicRequestRoundTripPreservesIdentityAndRequestID() throws {
        let original = try request()
        let data = try original.encoded()
        let restored = try WorkspaceKeyEnrollmentRequest(data: data)
        XCTAssertEqual(restored, original)
        XCTAssertEqual(try restored.encoded(), data)
        let root = try StrictJSON.decode(data).object()
        XCTAssertEqual(Set(root.keys), ["format", "version", "request_id", "key"])
        XCTAssertEqual(Set(try root.required("key").object().keys),
                       ["id", "algorithm", "public_blob", "public_fingerprint"])
        XCTAssertFalse(String(decoding: data, as: UTF8.self).contains("PRIVATE"))
        XCTAssertFalse(String(decoding: data, as: UTF8.self).contains("private_material"))
        XCTAssertEqual(restored.key.authorizedKey, try key().authorizedKey)
    }

    func testRequestRejectsDuplicateUnknownPrivateAndWrongVersionFields() throws {
        let data = try request().encoded()
        for field in ["private_key", "private_material", "targets", "execute", "extensions"] {
            let altered = try edited(data) { $0[field] = .string("not accepted") }
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentRequest(data: altered), field)
        }
        let extraKey = try edited(data) { root in
            var value = try root.required("key").object()
            value["private_container"] = .string("not accepted")
            root["key"] = .object(value)
        }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentRequest(data: extraKey))
        let versions: [JSONValue] = [.number("0"), .number("2"), .number("1.0"), .string("1")]
        for version in versions {
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentRequest(data: edited(data) { $0["version"] = version }))
        }
        let text = try XCTUnwrap(String(data: data, encoding: .utf8))
        let duplicate = text.replacingOccurrences(of: "\"version\":1", with: "\"version\":1,\"version\":1")
        XCTAssertNotEqual(duplicate, text)
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentRequest(data: Data(duplicate.utf8)))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentRequest(data: data + Data(" {}".utf8)))
    }

    func testRequestRejectsMismatchedFingerprintBlobAlgorithmAndPhoneIdentity() throws {
        let data = try request().encoded()
        let corruptions: [(String, JSONValue)] = [
            ("id", .string("mac-key")),
            ("id", .string("phone-key-00000000-0000-4000-8000-00000000000Z")),
            ("algorithm", .string("ssh-rsa")),
            ("public_blob", .string(Data(repeating: 1, count: 51).base64EncodedString())),
            ("public_blob", .string("AAAA\n")),
            ("public_fingerprint", .string("SHA256:" + String(repeating: "A", count: 43)))
        ]
        for (name, replacement) in corruptions {
            let altered = try edited(data) { root in
                var value = try root.required("key").object()
                value[name] = replacement; root["key"] = .object(value)
            }
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentRequest(data: altered), name)
        }
        let uppercase = try edited(data) { $0["request_id"] = .string("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA") }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentRequest(data: uppercase))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentRequest(key: key(id: "not-a-phone-key")))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentRequest(data: Data()))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentRequest(
            data: Data(repeating: 32, count: WorkspaceKeyEnrollmentRequest.maximumBytes + 1)))
    }

    func testOperationIDBindsRequestIdentityCanonicalHostPortAndAccount() throws {
        let request = try request(), original = try target()
        let operation = request.operationID(for: original)
        XCTAssertEqual(operation.count, 64)
        XCTAssertTrue(operation.utf8.allSatisfy { (48...57).contains($0) || (97...102).contains($0) })
        XCTAssertEqual(operation, request.operationID(for: try target("AGENT.EXAMPLE.")))
        XCTAssertEqual(try target("[2001:0db8::1]"), try target("2001:db8::1"))
        XCTAssertEqual(try target("::ffff:192.0.2.10"), try target("192.0.2.10"))
        XCTAssertNotEqual(operation, request.operationID(for: try target("other.example")))
        XCTAssertNotEqual(operation, request.operationID(for: try target(port: 2222)))
        XCTAssertNotEqual(operation, request.operationID(for: try target(user: "another")))
        XCTAssertNotEqual(operation, try self.request("20000000-0000-4000-8000-000000000001")
            .operationID(for: original))
        let otherIdentity = try WorkspaceKeyEnrollmentRequest(
            key: key(id: "phone-key-00000000-0000-4000-8000-000000000002"),
            requestID: XCTUnwrap(UUID(uuidString: request.id)))
        XCTAssertNotEqual(operation, otherIdentity.operationID(for: original))
    }

    func testTargetsRejectCommandSyntaxAmbiguousAddressesAndInvalidAccounts() throws {
        for host in ["-oProxyCommand=evil", "a b", "a\nb", "a/b", "user@host", "host%zone", "éxample",
                     "127.1", "0x7f000001", "01.2.3.4", "2001::db8::1", "$(id)", "a;id", "*.example"] {
            XCTAssertThrowsError(try target(host), host)
        }
        for user in ["", "-root", "root;id", "a b", "a\nb", "$(id)", "a@b", "root/other", "øperator",
                     String(repeating: "a", count: 129)] {
            XCTAssertThrowsError(try target(user: user), user)
        }
        XCTAssertThrowsError(try target(port: 0))
        XCTAssertEqual(try target(user: "_build.agent-1").user, "_build.agent-1")
        XCTAssertEqual(try target(port: UInt16.max).port, UInt16.max)
    }

    func testRouteScopedTargetsKeepPrivateNetworksAndLegacyReceiptsDistinct() throws {
        let request = try request(), direct = try target("internal.example")
        let first = try WorkspaceKeyEnrollmentTarget(host: direct.host, user: direct.user,
            routeScope: String(repeating: "a", count: 64))
        let second = try WorkspaceKeyEnrollmentTarget(host: direct.host, user: direct.user,
            routeScope: String(repeating: "b", count: 64))
        XCTAssertEqual(Set([direct.id, first.id, second.id]).count, 3)
        XCTAssertEqual(Set([direct, first, second].map { request.operationID(for: $0) }).count, 3)
        XCTAssertEqual(try WorkspaceKeyEnrollmentTarget(host: "INTERNAL.EXAMPLE.", user: direct.user,
            routeScope: first.routeScope), first)
        for invalid in ["", "abcd", String(repeating: "A", count: 64), String(repeating: "g", count: 64),
                        String(repeating: "0", count: 63), String(repeating: "0", count: 65)] {
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentTarget(host: direct.host, user: direct.user, routeScope: invalid))
        }
        XCTAssertEqual(try JSONDecoder().decode(WorkspaceKeyEnrollmentTarget.self,
            from: JSONEncoder().encode(first)), first)
        let nullScope = Data(#"{"host":"internal.example","port":22,"user":"operator","routeScope":null}"#.utf8)
        XCTAssertThrowsError(try JSONDecoder().decode(WorkspaceKeyEnrollmentTarget.self, from: nullScope))
    }

    func testVersionThreePreservesSeparateResultsForIdenticalEndpointsBehindDifferentRoutes() throws {
        let request = try request(), direct = try target()
        let first = try WorkspaceKeyEnrollmentTarget(host: direct.host, user: direct.user,
            routeScope: String(repeating: "a", count: 64))
        let second = try WorkspaceKeyEnrollmentTarget(host: direct.host, user: direct.user,
            routeScope: String(repeating: "b", count: 64))
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: [direct, first, second])
        try journal.beginInstall(targetID: first.id)
        try journal.record(result(request, first, .installed), action: .install)
        try journal.beginMutation(targetID: second.id, action: .remove)
        let bytes = try journal.encoded()
        XCTAssertEqual(try StrictJSON.decode(bytes).object().required("version").uint(), 3)
        let restored = try WorkspaceKeyEnrollmentJournal(data: bytes)
        XCTAssertEqual(restored, journal)
        XCTAssertNil(restored.result(for: direct.id))
        XCTAssertEqual(restored.result(for: first.id)?.state, .installed)
        XCTAssertEqual(restored.result(for: second.id)?.state, .uncertain)
        XCTAssertEqual(restored.lastMutation(for: second.id), .remove)
        let previous = journal
        XCTAssertThrowsError(try journal.record(.init(operationID: request.operationID(for: first),
            target: second, state: .removed, detail: "A receipt for another reviewed route."), action: .remove))
        XCTAssertEqual(journal, previous)
        for oldVersion in ["1", "2"] {
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: edited(bytes) {
                $0["version"] = .number(oldVersion)
            }), "A legacy reader cannot silently lose path identity.")
        }
        let directOnly = try WorkspaceKeyEnrollmentJournal(request: request, targets: [direct]).encoded()
        XCTAssertEqual(try StrictJSON.decode(directOnly).object().required("version").uint(), 2)
    }

    func testRouteScopeTamperingCannotMoveAnExistingReceiptToAnotherPath() throws {
        let request = try request()
        let destination = try WorkspaceKeyEnrollmentTarget(host: "internal.example", user: "operator",
            routeScope: String(repeating: "a", count: 64))
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: [destination])
        try journal.beginInstall(targetID: destination.id)
        let original = try journal.encoded()
        for replacement in [JSONValue.string(String(repeating: "b", count: 64)), .null, .string("invalid")] {
            let changed = try edited(original) { root in
                var targets = try root.required("targets").array()
                var target = try targets[0].object(); target["route_scope"] = replacement
                targets[0] = .object(target); root["targets"] = .array(targets)
            }
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: changed))
        }
        let stripped = try edited(original) { root in
            var targets = try root.required("targets").array()
            var target = try targets[0].object(); target["route_scope"] = nil
            targets[0] = .object(target); root["targets"] = .array(targets)
        }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: stripped))
    }

    func testCodableTargetAndResultRestoreThroughValidation() throws {
        let destination = try target()
        XCTAssertEqual(try JSONDecoder().decode(WorkspaceKeyEnrollmentTarget.self,
            from: JSONEncoder().encode(destination)), destination)
        let unsafe = Data(#"{"host":"-bad","port":22,"user":"operator"}"#.utf8)
        XCTAssertThrowsError(try JSONDecoder().decode(WorkspaceKeyEnrollmentTarget.self, from: unsafe))
        let extra = Data(#"{"host":"host.example","port":22,"user":"operator","proxy":"exec"}"#.utf8)
        XCTAssertThrowsError(try JSONDecoder().decode(WorkspaceKeyEnrollmentTarget.self, from: extra))
        let result = try result(request(), destination, .uncertain)
        XCTAssertEqual(try JSONDecoder().decode(WorkspaceKeyEnrollmentResult.self,
            from: JSONEncoder().encode(result)), result)
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentResult(operationID: "invalid", target: destination,
            state: .installed, detail: "Not admitted."))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentResult(operationID: result.operationID, target: destination,
            state: .installed, detail: "remote\u{001b}[31m"))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentResult(operationID: result.operationID, target: destination,
            state: .blocked, detail: String(repeating: "x", count: 1025)))
    }

    func testJournalPersistsUncertainBeforeEffectsAndReconcilesOnlySuccessfulReads() throws {
        let request = try request(), destination = try target()
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: [destination])
        XCTAssertNil(journal.result(for: destination.id))
        try journal.beginInstall(targetID: destination.id)
        let persistedBeforeSSH = try journal.encoded()
        var reopened = try WorkspaceKeyEnrollmentJournal(data: persistedBeforeSSH)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .uncertain)
        try reopened.record(result(request, destination, .blocked), action: .inspect)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .uncertain,
                       "A failed read cannot certify that an interrupted write failed.")
        try reopened.record(result(request, destination, .blocked), action: .install)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .uncertain,
                       "A blocked retry cannot erase an earlier edit's uncertain outcome.")
        try reopened.record(result(request, destination, .alreadyPresent), action: .inspect)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .alreadyPresent)
        try reopened.beginInstall(targetID: destination.id)
        XCTAssertThrowsError(try reopened.record(result(request, destination, .absent), action: .install))
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .uncertain,
                       "An install response cannot masquerade as a read-only absence observation.")
        try reopened.record(result(request, destination, .absent), action: .inspect)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .absent)
        XCTAssertEqual(try WorkspaceKeyEnrollmentJournal(data: reopened.encoded()), reopened)
    }

    func testJournalKeepsEachAccountOutcomeIndependentAndRejectsForeignResults() throws {
        let request = try request(), first = try target(), second = try target("second.example")
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: [first, second])
        try journal.beginInstall(targetID: first.id); try journal.beginInstall(targetID: second.id)
        try journal.record(result(request, second, .restricted), action: .install)
        try journal.record(result(request, first, .installed), action: .install)
        XCTAssertEqual(journal.results.map(\.target), [first, second])
        XCTAssertEqual(journal.results.map(\.state), [.installed, .restricted])
        let previous = journal
        XCTAssertThrowsError(try journal.record(result(self.request("20000000-0000-4000-8000-000000000001"),
                                                      first, .installed), action: .install))
        XCTAssertThrowsError(try journal.record(result(request, target("foreign.example"), .installed), action: .install))
        XCTAssertThrowsError(try journal.record(result(request, first, .installed), action: .inspect))
        XCTAssertThrowsError(try journal.beginInstall(targetID: String(repeating: "0", count: 64)))
        XCTAssertEqual(journal, previous)
    }

    func testJournalRejectsForgedOperationsDuplicateTargetsAndUnknownFields() throws {
        let request = try request(), destination = try target()
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: [destination])
        try journal.beginInstall(targetID: destination.id)
        let bytes = try journal.encoded()
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(request: request, targets: [destination, destination]))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(request: request, targets: []))
        let forged = try edited(bytes) { root in
            var rows = try root.required("results").array()
            var row = try rows[0].object()
            row["operation_id"] = .string(String(repeating: "0", count: 64))
            rows[0] = .object(row); root["results"] = .array(rows)
        }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: forged))
        let duplicate = try edited(bytes) { root in
            let rows = try root.required("results").array(); root["results"] = .array(rows + rows)
        }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: duplicate))
        let unknown = try edited(bytes) { $0["resume_automatically"] = .boolean(true) }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: unknown))
        let noTarget = try edited(bytes) { root in
            var rows = try root.required("results").array()
            var row = try rows[0].object()
            row["target_id"] = .string(String(repeating: "0", count: 64))
            rows[0] = .object(row); root["results"] = .array(rows)
        }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: noTarget))
        let wrongKey = try edited(bytes) { root in root["request"] = try StrictJSON.decode(self.request(
            "20000000-0000-4000-8000-000000000001").encoded()) }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: wrongKey))
        let text = try XCTUnwrap(String(data: bytes, encoding: .utf8))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: Data(
            text.replacingOccurrences(of: "\"results\":", with: "\"results\":[],\"results\":").utf8)))
    }

    func testJournalBoundsFleetAndSerializedInput() throws {
        let request = try request()
        let targets = try (0..<WorkspaceKeyEnrollmentJournal.maximumTargets).map { try target("agent-\($0).example") }
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: targets)
        for destination in targets { try journal.beginInstall(targetID: destination.id) }
        XCTAssertEqual(try WorkspaceKeyEnrollmentJournal(data: journal.encoded()), journal)
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(request: request,
            targets: targets + [target("overflow.example")]))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: Data()))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(
            data: Data(repeating: 32, count: WorkspaceKeyEnrollmentJournal.maximumBytes + 1)))
    }

    func testLostRemovalReplyRestoresIntentAndOnlySuccessfulReadSettlesUncertainty() throws {
        let request = try request(), destination = try target()
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: [destination])
        try journal.beginInstall(targetID: destination.id)
        try journal.record(result(request, destination, .installed), action: .install)
        try journal.beginMutation(targetID: destination.id, action: .remove)
        let persistedBeforeRemoteWrite = try journal.encoded()
        var reopened = try WorkspaceKeyEnrollmentJournal(data: persistedBeforeRemoteWrite)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .uncertain)
        XCTAssertEqual(reopened.lastMutation(for: destination.id), .remove)
        XCTAssertEqual(reopened.lastAction(for: destination.id), .remove)
        XCTAssertEqual(try reopened.encoded(), persistedBeforeRemoteWrite,
                       "Loading a pending removal must not execute it or manufacture a new outcome.")

        try reopened.record(result(request, destination, .blocked), action: .inspect)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .uncertain)
        XCTAssertEqual(reopened.lastMutation(for: destination.id), .remove)
        XCTAssertEqual(reopened.lastAction(for: destination.id), .inspect)
        try reopened.record(result(request, destination, .blocked), action: .remove)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .uncertain,
                       "A refused retry does not establish whether the earlier removal took effect.")
        reopened = try .init(data: reopened.encoded())
        XCTAssertEqual(reopened.lastMutation(for: destination.id), .remove)
        try reopened.record(result(request, destination, .absent), action: .inspect)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .absent,
                       "Readback proves absence, not that this process performed a removal.")
        XCTAssertEqual(reopened.lastAction(for: destination.id), .inspect)
        XCTAssertEqual(reopened.lastMutation(for: destination.id), .remove)
        XCTAssertEqual(try WorkspaceKeyEnrollmentJournal(data: reopened.encoded()), reopened)
    }

    func testIndependentAuthorizationSurvivesRemovalAndReconciliation() throws {
        let request = try request(), destination = try target()
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: [destination])
        try journal.beginMutation(targetID: destination.id, action: .remove)
        try journal.record(result(request, destination, .stillAuthorized), action: .remove)
        var reopened = try WorkspaceKeyEnrollmentJournal(data: journal.encoded())
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .stillAuthorized)
        XCTAssertEqual(reopened.lastAction(for: destination.id), .remove)
        try reopened.record(result(request, destination, .stillAuthorized), action: .inspect)
        XCTAssertEqual(reopened.lastAction(for: destination.id), .inspect)
        XCTAssertEqual(reopened.lastMutation(for: destination.id), .remove)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .stillAuthorized,
                       "Disabling our entry cannot claim that an independent authorization was removed.")
        try reopened.beginMutation(targetID: destination.id, action: .remove)
        try reopened.record(result(request, destination, .removed), action: .remove)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .removed)
        try reopened.beginMutation(targetID: destination.id, action: .remove)
        try reopened.record(result(request, destination, .absent), action: .remove)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .absent,
                       "A repeated removal can observe absence without claiming another mutation.")
        XCTAssertEqual(reopened.lastAction(for: destination.id), .remove)
        try reopened.record(result(request, destination, .absent), action: .inspect)
        XCTAssertEqual(reopened.result(for: destination.id)?.state, .absent)
        XCTAssertEqual(reopened.lastMutation(for: destination.id), .remove)
        XCTAssertEqual(try WorkspaceKeyEnrollmentJournal(data: reopened.encoded()), reopened)
    }

    func testMixedAccountIntentsAndReadOnlyObservationsSurviveCombinedExport() throws {
        let request = try request()
        let first = try target(), second = try target("second.example"), third = try target("third.example")
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: [first, second, third])
        try journal.beginMutation(targetID: second.id, action: .remove)
        try journal.record(result(request, third, .stillAuthorized), action: .inspect)
        try journal.beginInstall(targetID: first.id)
        try journal.record(result(request, first, .installed), action: .install)
        try journal.record(result(request, second, .absent), action: .inspect)
        XCTAssertEqual(journal.results.map(\.target), [first, second, third])
        XCTAssertEqual(journal.results.map(\.state), [.installed, .absent, .stillAuthorized])
        XCTAssertEqual(journal.lastMutation(for: first.id), .install)
        XCTAssertEqual(journal.lastMutation(for: second.id), .remove)
        XCTAssertNil(journal.lastMutation(for: third.id))
        XCTAssertNil(journal.lastAction(for: "not-a-target"))

        var combined = try WorkspaceKeyEnrollmentJournal(request: request, targets: journal.targets)
        try combined.mergeResults(from: journal)
        XCTAssertEqual(try WorkspaceKeyEnrollmentJournal(data: combined.encoded()), journal,
                       "Saving a fleet report must preserve each account's independent edit and readback intent.")
    }

    func testCombinedReportPreservesFailedInspectionAfterSettledRemoval() throws {
        let request = try request(), first = try target(), second = try target("second.example")
        var installed = try WorkspaceKeyEnrollmentJournal(request: request, targets: [first])
        try installed.beginInstall(targetID: first.id)
        try installed.record(result(request, first, .installed), action: .install)
        var removed = try WorkspaceKeyEnrollmentJournal(request: request, targets: [second])
        try removed.beginMutation(targetID: second.id, action: .remove)
        try removed.record(result(request, second, .removed), action: .remove)
        try removed.record(result(request, second, .blocked), action: .inspect)
        XCTAssertEqual(removed.result(for: second.id)?.state, .blocked,
                       "A failed inspection after a settled operation does not make that operation uncertain.")

        var report = try WorkspaceKeyEnrollmentJournal(request: request, targets: [first, second])
        try report.mergeResults(from: removed)
        try report.mergeResults(from: installed)
        let reopened = try WorkspaceKeyEnrollmentJournal(data: report.encoded())
        XCTAssertEqual(reopened.results.map(\.state), [.installed, .blocked])
        XCTAssertEqual(reopened.result(for: second.id), removed.result(for: second.id))
        XCTAssertEqual(reopened.lastAction(for: second.id), .inspect)
        XCTAssertEqual(reopened.lastMutation(for: second.id), .remove)
        XCTAssertEqual(reopened.lastAction(for: first.id), .install)
        XCTAssertEqual(reopened.lastMutation(for: first.id), .install)
    }

    func testCombinedReportRejectsOverlappingAndForeignReceiptsAtomically() throws {
        let request = try request(), first = try target(), second = try target("second.example")
        var report = try WorkspaceKeyEnrollmentJournal(request: request, targets: [first, second])
        try report.beginInstall(targetID: second.id)
        var source = try WorkspaceKeyEnrollmentJournal(request: request, targets: [first, second])
        try source.record(result(request, first, .absent), action: .inspect)
        try source.record(result(request, second, .absent), action: .inspect)
        let unchanged = report
        XCTAssertThrowsError(try report.mergeResults(from: source),
                             "An unordered older receipt must not overwrite a current uncertain operation.")
        XCTAssertEqual(report, unchanged, "The first source receipt must not leak through a rejected merge.")

        let otherRequest = try self.request("20000000-0000-4000-8000-000000000001")
        let foreignRequest = try WorkspaceKeyEnrollmentJournal(request: otherRequest, targets: [first])
        XCTAssertThrowsError(try report.mergeResults(from: foreignRequest))
        let foreignTarget = try WorkspaceKeyEnrollmentJournal(request: request, targets: [target("foreign.example")])
        XCTAssertThrowsError(try report.mergeResults(from: foreignTarget))
        XCTAssertEqual(report, unchanged)
        XCTAssertNil(report.result(for: first.id))
    }

    func testJournalRejectsMutationClaimsWithoutTheirMatchingBegunIntent() throws {
        let request = try request(), destination = try target()
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: [destination])
        let empty = journal
        XCTAssertThrowsError(try journal.beginMutation(targetID: destination.id, action: .inspect))
        XCTAssertThrowsError(try journal.record(result(request, destination, .removed), action: .remove))
        XCTAssertThrowsError(try journal.record(result(request, destination, .installed), action: .install))
        XCTAssertEqual(journal, empty)

        try journal.beginMutation(targetID: destination.id, action: .remove)
        let pendingRemoval = journal
        for state: WorkspaceKeyEnrollmentState in [.installed, .alreadyPresent, .restricted] {
            XCTAssertThrowsError(try journal.record(result(request, destination, state), action: .remove))
        }
        XCTAssertThrowsError(try journal.record(result(request, destination, .installed), action: .install),
                             "An old installation completion cannot replace a newer removal decision.")
        XCTAssertThrowsError(try journal.record(result(request, destination, .removed), action: .inspect))
        XCTAssertThrowsError(try journal.beginMutation(targetID: "foreign-target", action: .remove))
        XCTAssertEqual(journal, pendingRemoval)

        try journal.beginInstall(targetID: destination.id)
        let pendingInstall = journal
        for state: WorkspaceKeyEnrollmentState in [.absent, .removed, .stillAuthorized] {
            XCTAssertThrowsError(try journal.record(result(request, destination, state), action: .install))
        }
        XCTAssertThrowsError(try journal.record(result(request, destination, .removed), action: .remove))
        XCTAssertEqual(journal, pendingInstall)
        try journal.record(result(request, destination, .restricted), action: .install)
        XCTAssertEqual(journal.lastMutation(for: destination.id), .install)
    }

    private func legacyJournal(_ request: WorkspaceKeyEnrollmentRequest,
                               observations: [(WorkspaceKeyEnrollmentTarget, WorkspaceKeyEnrollmentState)]) throws -> Data {
        try StrictJSON.encode(.object([
            "format": .string("ft-phone-key-enrollment-journal"), "version": .number("1"),
            "request": try StrictJSON.decode(request.encoded()),
            "targets": .array(observations.map { target, _ in .object([
                "host": .string(target.host), "port": .number(String(target.port)), "user": .string(target.user)
            ]) }),
            "results": .array(observations.map { target, state in .object([
                "operation_id": .string(request.operationID(for: target)), "target_id": .string(target.id),
                "state": .string(state.rawValue), "detail": .string("A saved version-one receipt.")
            ]) })
        ]))
    }

    func testVersionOneReceiptsMigrateWithoutInventingUnknownMutationHistory() throws {
        let request = try request()
        let observations = try [WorkspaceKeyEnrollmentState.installed, .uncertain, .absent,
                                .alreadyPresent, .restricted, .blocked].enumerated().map { index, state in
            (try target("legacy-\(index).example"), state)
        }
        let migrated = try WorkspaceKeyEnrollmentJournal(data: legacyJournal(request, observations: observations))
        XCTAssertEqual(migrated.results.map(\.state), observations.map(\.1))
        for (target, state) in observations {
            let knownInstall = state == .installed || state == .uncertain
            XCTAssertEqual(migrated.lastMutation(for: target.id), knownInstall ? .install : nil)
            XCTAssertEqual(migrated.lastAction(for: target.id), knownInstall ? .install : .inspect)
        }
        let upgraded = try migrated.encoded()
        XCTAssertEqual(try StrictJSON.decode(upgraded).object().required("version").uint(), 2)
        XCTAssertEqual(try WorkspaceKeyEnrollmentJournal(data: upgraded), migrated)

        var resumed = migrated
        let previouslyUncertain = observations[1].0
        try resumed.record(result(request, previouslyUncertain, .blocked), action: .inspect)
        XCTAssertEqual(resumed.result(for: previouslyUncertain.id)?.state, .uncertain)
        XCTAssertEqual(resumed.lastMutation(for: previouslyUncertain.id), .install)
        try resumed.beginMutation(targetID: previouslyUncertain.id, action: .remove)
        try resumed.record(result(request, previouslyUncertain, .removed), action: .remove)
        XCTAssertEqual(resumed.lastMutation(for: previouslyUncertain.id), .remove)
        XCTAssertEqual(try WorkspaceKeyEnrollmentJournal(data: resumed.encoded()), resumed)
    }

    func testVersionOneCannotSmuggleRemovalReceiptsOrVersionTwoAuthority() throws {
        let request = try request(), destination = try target()
        for state: WorkspaceKeyEnrollmentState in [.removed, .stillAuthorized] {
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(
                data: legacyJournal(request, observations: [(destination, state)])))
        }
        let original = try legacyJournal(request, observations: [(destination, .uncertain)])
        let forged = try edited(original) { root in
            var rows = try root.required("results").array()
            var row = try rows[0].object()
            row["last_mutation"] = .string("remove")
            rows[0] = .object(row); root["results"] = .array(rows)
        }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: forged))
    }

    func testVersionTwoRejectsMissingOrContradictoryReceiptProvenance() throws {
        let request = try request(), destination = try target()
        var journal = try WorkspaceKeyEnrollmentJournal(request: request, targets: [destination])
        try journal.beginMutation(targetID: destination.id, action: .remove)
        let original = try journal.encoded()
        let corruptions: [(String, JSONValue?)] = [
            ("last_mutation", nil), ("action", nil),
            ("last_mutation", .null), ("last_mutation", .string("install")),
            ("last_mutation", .string("inspect")), ("last_mutation", .string("execute")),
            ("last_mutation", .object([:])), ("action", .string("execute")),
            ("action", .string("install")), ("state", .string("installed")),
            ("state", .string("alreadyPresent")), ("state", .string("restricted"))
        ]
        for (name, value) in corruptions {
            let forged = try edited(original) { root in
                var rows = try root.required("results").array()
                var row = try rows[0].object()
                row[name] = value
                rows[0] = .object(row); root["results"] = .array(rows)
            }
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: forged), "\(name): \(String(describing: value))")
        }
        let forgedInspect = try edited(original) { root in
            var rows = try root.required("results").array()
            var row = try rows[0].object()
            row["action"] = .string("inspect"); row["state"] = .string("removed")
            rows[0] = .object(row); root["results"] = .array(rows)
        }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: forgedInspect))
        for version in [JSONValue.number("0"), .number("4"), .number("2.0"), .string("2")] {
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentJournal(data: edited(original) { $0["version"] = version }))
        }
    }
}
