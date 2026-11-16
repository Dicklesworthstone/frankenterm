import Foundation
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

/// A public certificate renewed on this phone. It changes neither the imported
/// desktop document nor its protected raw private-key binding.
public struct WorkspaceCertificateRenewal: Codable, Equatable, Sendable {
    public static let maximumFileBytes = 64 * 1024
    public let credentialID: String
    public let certificate: Data
    let sourceWitness: Data
}

public enum WorkspaceCertificateRenewalError: Error, Equatable, Sendable, LocalizedError {
    case invalidFile, unavailableCredential, subjectChanged, issuerChanged, policyChanged, notNewer, expiredReview

    public var errorDescription: String? {
        switch self {
        case .invalidFile:
            "Choose one public OpenSSH user-certificate file containing its algorithm and base64 certificate."
        case .unavailableCredential:
            "This saved credential is not an imported user certificate used by a supported SSH route."
        case .subjectChanged:
            "The renewed certificate belongs to a different private key. Select the certificate for this exact credential."
        case .issuerChanged:
            "The certificate was signed by a different authority. Changing its issuer requires a reviewed workspace update."
        case .policyChanged:
            "The certificate changes account principals, restrictions, or permissions. Import a reviewed workspace update for that policy change."
        case .notNewer:
            "The certificate must extend the current certificate's expiry without moving its start time backward."
        case .expiredReview:
            "This certificate review expired. Choose and review the current certificate again."
        }
    }
}

/// Issued by the store for one still-current credential and public certificate.
/// This is not permission to connect, sign, or read protected private material.
public struct WorkspaceCertificateRenewalReview: Identifiable, Sendable {
    public let id: UUID
    public let workspaceID: UUID, expectedRevision: UUID
    public let workspaceName: String, credentialID: String
    public let currentCertificate: SSHUserCertificate, renewedCertificate: SSHUserCertificate
    public let affectedRouteIDs: [String]
    let sourceWitness: Data
    let owner: UUID
    let expires: ContinuousClock.Instant
}

enum WorkspaceCertificateRenewalOverlay {
    static let limit = 256
    static let maximumCertificateBytes = 1024 * 1024

    /// The file's comment is untrusted decoration and is never retained or
    /// displayed. Multiple keys, authorized_keys options, private keys, and
    /// algorithm labels that disagree with the signed blob are refused.
    static func readPublicFile(_ data: Data) throws -> SSHUserCertificate {
        guard !data.isEmpty, data.count <= WorkspaceCertificateRenewal.maximumFileBytes,
              let text = String(data: data, encoding: .utf8),
              !text.unicodeScalars.contains(where: {
                  CharacterSet.controlCharacters.contains($0) && $0 != "\n" && $0 != "\r" && $0 != "\t"
              }) else { throw WorkspaceCertificateRenewalError.invalidFile }
        let lines = text.split(whereSeparator: { $0 == "\n" || $0 == "\r" })
            .filter { !$0.allSatisfy({ $0 == " " || $0 == "\t" }) }
        guard lines.count == 1 else { throw WorkspaceCertificateRenewalError.invalidFile }
        let fields = lines[0].split(maxSplits: 2, whereSeparator: { $0 == " " || $0 == "\t" })
        guard fields.count >= 2, fields[0].utf8.count <= 64,
              fields[1].utf8.count <= FTLimit.certificateText,
              let blob = Data(base64Encoded: String(fields[1])),
              blob.base64EncodedString().utf8.elementsEqual(fields[1].utf8) else {
            throw WorkspaceCertificateRenewalError.invalidFile
        }
        let certificate = try SSHUserCertificate(blob: blob)
        guard certificate.algorithm.utf8.elementsEqual(fields[0].utf8) else {
            throw WorkspaceCertificateRenewalError.invalidFile
        }
        return certificate
    }

    static func validatePolicy(_ renewal: SSHUserCertificate, replacing original: SSHUserCertificate) throws {
        guard renewal.subject.blob == original.subject.blob else { throw WorkspaceCertificateRenewalError.subjectChanged }
        guard renewal.authority.blob == original.authority.blob else { throw WorkspaceCertificateRenewalError.issuerChanged }
        guard Set(renewal.principals.map { Data($0.utf8) }) == Set(original.principals.map { Data($0.utf8) }),
              original.criticalOptions.isEmpty, renewal.criticalOptions.isEmpty,
              renewal.extensionOptions == original.extensionOptions else {
            throw WorkspaceCertificateRenewalError.policyChanged
        }
        guard renewal.validBefore > original.validBefore, renewal.validAfter >= original.validAfter else {
            throw WorkspaceCertificateRenewalError.notNewer
        }
    }

    static func affectedRoutes(credentialID: String, in document: WorkspaceDocument) throws -> [WorkspaceDocument.Route] {
        let routes = document.routes.filter { $0.credentialIDs.contains(credentialID) }
        guard !routes.isEmpty, routes.allSatisfy({ $0.mode == "direct_ssh" || $0.mode == "frankenterm_mux" }) else {
            throw WorkspaceCertificateRenewalError.unavailableCredential
        }
        return routes.sorted { $0.id < $1.id }
    }

    /// Derive real destination and transit roles. A bastion used only for
    /// forwarding must not suddenly require a PTY merely to renew its key.
    static func validateUse(_ certificate: SSHUserCertificate, credentialID: String,
                            in document: WorkspaceDocument, now: Date) throws -> [String] {
        let routes = try affectedRoutes(credentialID: credentialID, in: document)
        let destinations = Set(document.machines.flatMap(\.routeIDs))
        var forwarding = Set<String>()
        for id in destinations {
            forwarding.formUnion(try document.orderedJumpRoutes(forRouteID: id).map(\.id))
        }
        for route in routes {
            try certificate.validateAuthentication(user: route.user, now: now,
                requiresPTY: destinations.contains(route.id) && route.mode == "direct_ssh",
                requiresPortForwarding: forwarding.contains(route.id))
        }
        return routes.map(\.id)
    }

    private struct PolicyIndex {
        let document: WorkspaceDocument
        let credentials: [String: WorkspaceDocument.Credential]
        let routeWitnesses: [String: Data]

        init(_ document: WorkspaceDocument) throws {
            self.document = document
            let indexed = Dictionary(uniqueKeysWithValues: document.credentials.map { ($0.id, $0) })
            credentials = indexed
            let routeIDs = Set(document.routes.filter { route in
                route.credentialIDs.contains { id in indexed[id]?.userCertificate != nil }
            }.map(\.id))
            routeWitnesses = try WorkspaceDeviceKeyOverlay.certificateRenewalWitnesses(routeIDs: routeIDs, in: document)
        }

        func witness(credentialID: String) throws -> Data {
            guard let credential = credentials[credentialID], let original = credential.userCertificate else {
                throw WorkspaceCertificateRenewalError.unavailableCredential
            }
            let routes = try affectedRoutes(credentialID: credentialID, in: document)
            let witnesses = try routes.map { route -> JSONValue in
                guard let witness = routeWitnesses[route.id] else { throw WorkspaceStoreError.corruptStore }
                return .object(["route": .string(route.id), "policy": .string(witness.base64EncodedString())])
            }
            return Data(SHA256.hash(data: try StrictJSON.encode(.object([
                "credential": .string(credentialID), "certificate": .string(original.fingerprint),
                "routes": .array(witnesses)
            ]))))
        }
    }

    static func witness(credentialID: String, in imported: WorkspaceDocument) throws -> Data {
        try PolicyIndex(imported).witness(credentialID: credentialID)
    }

    /// A source-policy change permanently removes its old local renewal during
    /// import. Reintroducing that old source later cannot reactivate the choice.
    static func retained(_ renewals: [WorkspaceCertificateRenewal], in imported: WorkspaceDocument) throws -> [WorkspaceCertificateRenewal] {
        guard renewals.count <= limit, Set(renewals.map(\.credentialID)).count == renewals.count,
              renewals.reduce(0, { $0 + $1.certificate.count }) <= maximumCertificateBytes else {
            throw WorkspaceStoreError.corruptStore
        }
        guard !renewals.isEmpty else { return [] }
        let policy = try PolicyIndex(imported)
        var retained: [WorkspaceCertificateRenewal] = []
        for renewal in renewals {
            guard (try? WorkspaceDocument.identifier(renewal.credentialID)) != nil,
                  renewal.sourceWitness.count == 32,
                  let certificate = try? SSHUserCertificate(blob: renewal.certificate) else {
                throw WorkspaceStoreError.corruptStore
            }
            guard let original = policy.credentials[renewal.credentialID]?.userCertificate,
                  let witness = try? policy.witness(credentialID: renewal.credentialID),
                  witness == renewal.sourceWitness else { continue }
            do { try validatePolicy(certificate, replacing: original) }
            catch { throw WorkspaceStoreError.corruptStore }
            retained.append(renewal)
        }
        return retained
    }

    static func document(effective: WorkspaceDocument, imported: WorkspaceDocument,
                         renewals: [WorkspaceCertificateRenewal]) throws -> WorkspaceDocument {
        guard !renewals.isEmpty else { return effective }
        let retained = try retained(renewals, in: imported)
        guard retained == renewals else { throw WorkspaceStoreError.corruptStore }
        let byID = Dictionary(uniqueKeysWithValues: renewals.map { ($0.credentialID, $0) })
        var root = try StrictJSON.decode(effective.raw).object()
        var workspace = try root.required("workspace").object()
        workspace["credentials"] = .array(try workspace.required("credentials").array().map { value in
            var credential = try value.object()
            guard let renewal = byID[try credential.text("id")] else { return value }
            credential["user_certificate"] = .string(renewal.certificate.base64EncodedString())
            return .object(credential)
        })
        root["workspace"] = .object(workspace)
        return try WorkspaceDocument(data: StrictJSON.encode(.object(root)))
    }
}
