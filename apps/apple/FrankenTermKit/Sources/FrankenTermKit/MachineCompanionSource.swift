import Foundation

/// The bounded local helper response used by the Mac's default export journey.
/// File references remain local selection hints; only the public document and
/// validated completeness findings enter an encrypted workspace archive.
public struct MachineCompanionSource: Sendable {
    public static let maximumBytes = 2 * FTLimit.manifest
    public static let maximumFindingsBytes = 256 * 1024
    public static let scopeSetting = "machines_toml.source_scope"
    public static let desktopScopeSetting = "desktop_snapshot.source_scope"

    /// Provenance of an immutable handoff from the running desktop. These
    /// fields describe the source; they do not authenticate imported SSH trust.
    public struct Runtime: Equatable, Sendable {
        public let instanceID: String
        public let configurationGeneration: UInt64
        public let configurationRevision: String
    }

    public let workspace: WorkspaceDocument
    public let inventory: String
    public let identityFiles: [String: String]
    public let findings: [AirGapArchive.Portability]
    public let runtime: Runtime?

    public init(data: Data, requireRunningDesktop: Bool = false) throws {
        let root = try StrictJSON.decodeMachineCompanion(data).object()
        guard try root.text("format") == "ftmachines-source" else { throw FTError.unsupported("machines.source") }
        let version = try root.required("version").uint()
        guard (1...3).contains(version) else { throw FTError.unsupported("machines.source_version") }
        guard !requireRunningDesktop || version == 3 else { throw FTError.unsupported("machines.running_source_required") }
        try root.allowing(version == 1
            ? ["format", "version", "inventory", "document", "identity_files", "notes"]
            : version == 2
                ? ["format", "version", "inventory", "document", "identity_files", "portability"]
                : ["format", "version", "inventory", "document", "identity_files", "portability", "runtime"])
        let workspace = try WorkspaceDocument(data: StrictJSON.encode(root.required("document")))
        let runtime: Runtime?
        if version == 3 {
            let witness = try root.required("runtime").object()
            try witness.allowing(["instance_id", "configuration_generation", "configuration_revision"])
            let instance = try witness.text("instance_id")
            let generation = try witness.required("configuration_generation").uint()
            let revision = try witness.text("configuration_revision")
            let prefix = "desktop:\(instance):\(generation):sha256:"
            guard instance.utf8.count == 32, instance.utf8.allSatisfy(Self.lowerHex), generation > 0,
                  revision.hasPrefix(prefix), revision.utf8.count == prefix.utf8.count + 64,
                  revision.utf8.dropFirst(prefix.utf8.count).allSatisfy(Self.lowerHex),
                  workspace.sourceKind == "desktop_snapshot", workspace.configurationRevision == revision else {
                throw FTError.malformed("machines.runtime_witness")
            }
            runtime = .init(instanceID: instance, configurationGeneration: generation, configurationRevision: revision)
        } else {
            guard workspace.sourceKind != "desktop_snapshot" else { throw FTError.malformed("machines.runtime_witness") }
            runtime = nil
        }
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
                $0.entityID == workspace.id && $0.setting == (runtime == nil ? Self.scopeSetting : Self.desktopScopeSetting)
                    && $0.disposition == 0
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
        self.runtime = runtime
    }

    private static func lowerHex(_ byte: UInt8) -> Bool { (48...57).contains(byte) || (97...102).contains(byte) }

    private static func text(_ value: String) throws {
        guard !value.isEmpty, value.utf8.count <= FTLimit.text,
              !value.unicodeScalars.contains(where: { $0.value < 32 || (127...159).contains($0.value) }) else {
            throw FTError.malformed("machines.source_text")
        }
    }
}
