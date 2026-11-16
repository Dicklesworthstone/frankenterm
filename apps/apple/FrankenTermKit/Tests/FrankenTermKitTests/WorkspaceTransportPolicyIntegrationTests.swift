import Foundation
import XCTest
@testable import FrankenTermKit

/// New transport restrictions must also participate in review, local credential
/// assignments and Mac enrollment, which consume the shared trust model.
final class WorkspaceTransportPolicyIntegrationTests: XCTestCase {
    private let restrictions: [(String, [String])] = [
        ("kex_algorithms", ["ecdh-sha2-nistp384"]),
        ("ciphers", ["aes256-ctr"]),
        ("macs", ["hmac-sha2-512-etm@openssh.com"])
    ]

    private func document(field: String? = nil, values: [String] = [], alias: Bool = false) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest()) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var trusts = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
        if let field {
            if alias {
                var related = trusts[0]; related["id"] = "unused-policy-alias"; related[field] = values
                trusts.append(related)
            } else { trusts[0][field] = values }
        }
        workspace["host_trust"] = trusts; root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    func testAddingAndRemovingEveryTransportDimensionRequiresSecurityReview() throws {
        let original = try document()
        for (field, values) in restrictions {
            let restricted = try document(field: field, values: values)
            for (before, after) in [(original, restricted), (restricted, original)] {
                let review = try WorkspaceChangeReview(previous: before, incoming: after)
                XCTAssertTrue(review.requiresSecurityApproval, field)
                let change = try XCTUnwrap(review.changes.first { $0.entity == .hostTrust && $0.field == field })
                XCTAssertTrue(change.affectsSecurity, field)
                XCTAssertEqual(review.securityChanges.count, 1, field)
            }
        }
    }

    func testUnselectedAliasTransportPolicyRetiresLocalKeyAndRenewalWitnesses() throws {
        let original = try document()
        let assignment = try WorkspaceDeviceKeyOverlay.assignments(routeIDs: ["route"], credentialID: "phone-key", in: original)
        XCTAssertTrue(try WorkspaceDeviceKeyOverlay.reviewRequiredRouteIDs(imported: original, assignments: assignment).isEmpty)
        let renewal = try WorkspaceDeviceKeyOverlay.certificateRenewalWitnesses(routeIDs: ["route"], in: original)
        for (field, values) in restrictions {
            let restricted = try document(field: field, values: values, alias: true)
            XCTAssertEqual(try WorkspaceDeviceKeyOverlay.reviewRequiredRouteIDs(imported: restricted, assignments: assignment),
                           ["route"], field)
            XCTAssertNotEqual(try WorkspaceDeviceKeyOverlay.certificateRenewalWitnesses(routeIDs: ["route"], in: restricted),
                              renewal, field)
        }
    }

    func testMacEnrollmentCannotBypassTransportPolicyWithoutAnExplicitCapabilityMarker() throws {
        let original = try document()
        XCTAssertNoThrow(try WorkspaceKeyEnrollmentSSH.plan(for: original.routes[0], in: original))
        for (field, values) in restrictions {
            for alias in [false, true] {
                let restricted = try document(field: field, values: values, alias: alias)
                XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.plan(for: restricted.routes[0], in: restricted), field) {
                    XCTAssertEqual($0 as? WorkspaceKeyEnrollmentSSHError, .trust)
                }
            }
        }
    }
}
