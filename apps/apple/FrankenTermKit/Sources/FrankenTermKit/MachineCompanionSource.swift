import Foundation

/// The bounded local helper response used by the Mac's default export journey.
/// File references remain local selection hints; only the public document and
/// validated completeness findings enter an encrypted workspace archive.
public struct MachineCompanionSource: Sendable {
    public static let maximumBytes = 2 * FTLimit.manifest
    public static let maximumFindingsBytes = 256 * 1024
    public static let scopeSetting = "machines_toml.source_scope"

    public let workspace: WorkspaceDocument
    public let inventory: String
    public let identityFiles: [String: String]
    public let findings: [AirGapArchive.Portability]

    public init(data: Data) throws {
        let root = try StrictJSON.decodeMachineCompanion(data).object()
        guard try root.text("format") == "ftmachines-source" else { throw FTError.unsupported("machines.source") }
        let version = try root.required("version").uint()
        guard version == 1 || version == 2 else { throw FTError.unsupported("machines.source_version") }
        try root.allowing(version == 1
            ? ["format", "version", "inventory", "document", "identity_files", "notes"]
            : ["format", "version", "inventory", "document", "identity_files", "portability"])
        let workspace = try WorkspaceDocument(data: StrictJSON.encode(root.required("document")))
        let inventory = try root.text("inventory")
        try Self.text(inventory)
        let references = try root.required("identity_files").object()
        guard references.count <= 256 else { throw FTError.limit("machines.identity_files") }
        let credentialIDs = Set(workspace.credentials.map(\.id))
        var files: [String: String] = [:]
        for (id, value) in references {
            guard credentialIDs.contains(id) else { throw FTError.malformed("machines.identity_reference") }
            let path = try value.string()
            try Self.text(path)
            guard path.hasPrefix("/") || path.hasPrefix("~/"),
                  !path.contains("%"), !path.contains("$") else {
                throw FTError.malformed("machines.identity_path")
            }
            files[id] = path
        }
        let findings: [AirGapArchive.Portability]
        if version == 1 {
            // Older helpers supply no entity scope. Keep the complete report
            // on the workspace so narrowing groups cannot silently erase it.
            let notes = try root.required("notes").array()
            guard notes.count < 1024 else { throw FTError.limit("archive.metadata") }
            var legacy: [AirGapArchive.Portability] = [
                .init(entityID: workspace.id, setting: Self.scopeSetting, disposition: 0,
                    reason: "This machine-inventory snapshot came from an older local helper. Its source notes are preserved for review; it does not capture running desktop configuration overrides or live sessions.")
            ]
            for (index, value) in notes.enumerated() {
                let reason = try value.string()
                try Self.text(reason)
                legacy.append(.init(entityID: workspace.id, setting: "machines_toml.legacy_note.\(index)",
                                    disposition: 0, reason: reason))
            }
            findings = legacy
        } else {
            let records = try root.required("portability").array()
            guard records.count <= 1024 else { throw FTError.limit("archive.metadata") }
            findings = try records.map { value in
                let fields = try value.object()
                try fields.allowing(["entity_id", "setting", "disposition", "reason"])
                return try .init(entityID: fields.text("entity_id"), setting: fields.text("setting"),
                                 disposition: fields.required("disposition").uint(), reason: fields.text("reason"))
            }
            guard findings.contains(where: {
                $0.entityID == workspace.id && $0.setting == Self.scopeSetting && $0.disposition == 0
            }) else { throw FTError.malformed("machines.source_scope") }
        }
        let metadata = try AirGapArchive.validatedMetadata(workspace: workspace, portability: findings, annotations: [])
        // Bound the whole report, including JSON escaping, before preview or
        // key selection. There is no usable-prefix fallback for excess notes.
        let encoded = try StrictJSON.encode(.array(metadata.portability.map { finding in
            .object(["entity_id": .string(finding.entityID), "setting": .string(finding.setting),
                     "disposition": .number(String(finding.disposition)), "reason": .string(finding.reason)])
        }))
        guard encoded.count <= Self.maximumFindingsBytes else { throw FTError.limit("machines.source_findings") }
        self.workspace = workspace; self.inventory = inventory
        identityFiles = files; self.findings = metadata.portability
    }

    private static func text(_ value: String) throws {
        guard !value.isEmpty, value.utf8.count <= FTLimit.text,
              !value.unicodeScalars.contains(where: { $0.value < 32 || (127...159).contains($0.value) }) else {
            throw FTError.malformed("machines.source_text")
        }
    }
}
