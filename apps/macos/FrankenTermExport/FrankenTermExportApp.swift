import SwiftUI
import Observation
import AppKit
import UniformTypeIdentifiers
import ImageIO
import Vision
@preconcurrency import AVFoundation
import FrankenTermKit

private typealias SourceKeyUnlockRequest = OpenSSHSourceKeyUnlocker<AppleCryptography>.Request

private enum ExportSourceSheet: Identifiable {
    case sshHosts(UUID), keyUnlock(SourceKeyUnlockRequest), recipientCamera(UUID)
    var id: UUID {
        switch self {
        case let .sshHosts(id): return id
        case let .keyUnlock(request): return request.id
        case let .recipientCamera(id): return id
        }
    }
}

private enum ExportProtection: Sendable, Hashable {
    case code, recipient
}

private enum MachineSourceError: LocalizedError {
    case noCommand, failed(String)
    var errorDescription: String? {
        switch self {
        case .noCommand:
            return "This Mac's FrankenTerm machines need the bundled frankenterm-machines helper (scripts/build-apple-machines-helper.sh) or an ft with `ft mobile source`."
        case let .failed(detail): return detail
        }
    }
}

@main
@MainActor
struct FrankenTermExportApp: App {
    @NSApplicationDelegateAdaptor(ExportApplicationDelegate.self) private var applicationDelegate
    @State private var model = MacExportModel()
    @State private var enrollment = MacPhoneKeyEnrollmentModel()
    var body: some Scene {
        Window("FrankenTerm Export", id: "export") {
            MacExportView(model: model, enrollment: enrollment)
                .onAppear {
                    applicationDelegate.retireOwnedWork = {
                        model.closeExport()
                        await enrollment.closeAndWait()
                        await model.closeAndWait()
                    }
                }
        }.defaultSize(width: 850, height: 900)
    }
}

@MainActor
private final class ExportApplicationDelegate: NSObject, NSApplicationDelegate {
    var retireOwnedWork: (@MainActor () async -> Void)?
    private var retiring = false
    private var retired = false

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard !retired, let retireOwnedWork else { return .terminateNow }
        if !retiring {
            retiring = true
            Task { @MainActor in
                await retireOwnedWork()
                retired = true
                sender.reply(toApplicationShouldTerminate: true)
            }
        }
        return .terminateLater
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        // A deliberately spawned desktop handoff owns one window/process. This
        // also releases the desktop's bounded child-process admission on close.
        CommandLine.arguments.contains("--desktop-source-stdin")
    }
}

/// File reads, parsing, key validation, compression and encryption remain off UI.
/// SSH source selection reads the chosen user config, bounded local includes,
/// referenced public identities, and each route's local known-hosts files.
/// Private keys need a separate pick.
private actor MacExportEngine {
    struct Prepared: Sendable {
        let id: UUID
        let comparison: String, pages: Int, bytes: Int
        let protection: ExportProtection
        let recipientComparison: String?
        let hasTransferCode: Bool
        let isReplay: Bool
    }
    struct SelectionState: Sendable {
        let source: WorkspaceDocument
        let selection: WorkspaceExportSelection
        let decisions: [String: WorkspaceExportSelection.IdentityDecision]
        let loadedFingerprints: Set<String>
        let review: WorkspaceExportSelection.Review
        let identityReferences: [String: String]
    }
    enum KeyLoadResult: Sendable {
        case loaded(SelectionState), passphraseRequired(SourceKeyUnlockRequest)
    }
    private struct PendingKeyInstallation {
        let requestID: UUID?
        let previousKeys: [PrivateKeyRecord]
        let previousDecisions: [String: WorkspaceExportSelection.IdentityDecision]
    }
    private let crypto = AppleCryptography()
    private var keyUnlocker = OpenSSHSourceKeyUnlocker(using: AppleCryptography())
    private var sourceKeyOwner: UUID?
    private var pendingKeyInstallation: PendingKeyInstallation?
    private var document: WorkspaceDocument?
    private var selection: WorkspaceExportSelection?
    private var excludedMachineIDs = Set<String>()
    private var sourceFindings: [AirGapArchive.Portability] = []
    private var identityReferences: [String: String] = [:]
    private var sshSource: (source: SSHWorkspaceSource, url: URL)?
    private var decisions: [String: WorkspaceExportSelection.IdentityDecision] = [:]
    private var keys: [PrivateKeyRecord] = []
    private var transfer: OpticalExportSession?

    private func fingerprint(_ key: PrivateKeyRecord) -> String {
        "SHA256:" + crypto.sha256(key.publicBlob).base64EncodedString().replacingOccurrences(of: "=", with: "")
    }
    private func state() throws -> SelectionState {
        guard let document, let selection else { throw FTError.stale }
        let loaded = Set(keys.map(fingerprint))
        return .init(source: document, selection: selection, decisions: decisions, loadedFingerprints: loaded,
                     review: try selection.review(decisions: decisions, loadedFingerprints: loaded),
                     identityReferences: identityReferences)
    }
    func invalidateExport() { transfer = nil }
    private func retireKeyRequest() {
        if let pending = pendingKeyInstallation {
            keys = pending.previousKeys; decisions = pending.previousDecisions
        }
        pendingKeyInstallation = nil
        try? keyUnlocker.cancel(); sourceKeyOwner = nil
    }
    func loadWorkspace(_ url: URL) throws -> SelectionState {
        retireKeyRequest(); invalidateExport()
        let candidate = try WorkspaceDocument(data: SelectedLocalFile.read(url, limit: FTLimit.manifest))
        let selected = try WorkspaceExportSelection(workspace: candidate, groupIDs: [candidate.initialGroupID])
        document = candidate; selection = selected; decisions = [:]; keys = []; invalidateExport()
        excludedMachineIDs = []
        sourceFindings = []; identityReferences = [:]; sshSource = nil
        return try state()
    }
    struct MachineSourceResult: Sendable {
        let state: SelectionState
        let inventory: String
        let runtime: MachineCompanionSource.Runtime?
        /// Credentials whose key file needs a passphrase or could not be used.
        let unresolved: [(credentialID: String, reason: String)]
    }
    /// What prints this Mac's machines: the helper bundled beside this app,
    /// else an `ft` new enough to have `ft mobile source`.
    static func machineSourceCommand() -> (executable: URL, arguments: [String])? {
        if let helper = Bundle.main.url(forAuxiliaryExecutable: "frankenterm-machines") { return (helper, ["source"]) }
        let home = FileManager.default.homeDirectoryForCurrentUser
        let candidates = [home.appendingPathComponent(".local/bin/ft"), URL(fileURLWithPath: "/opt/homebrew/bin/ft"),
                          URL(fileURLWithPath: "/usr/local/bin/ft"),
                          URL(fileURLWithPath: "/Applications/FrankenTerm.app/Contents/MacOS/ft")]
        return candidates.first { FileManager.default.isExecutableFile(atPath: $0.path) }.map { ($0, ["mobile", "source"]) }
    }
    private static func run(_ executable: URL, _ arguments: [String], limit: Int) throws -> Data {
        do {
            return try LocalHelperProcess.run(executable: executable, arguments: arguments,
                                               maximumOutputBytes: limit)
        } catch let error as LocalHelperProcessError {
            throw MachineSourceError.failed("\(executable.lastPathComponent): \(error.localizedDescription)")
        }
    }
    /// This Mac's FrankenTerm machines with every group selected, and every
    /// key their routes use included, read from the files the desktop's own
    /// ssh uses. A key that needs a passphrase is left for an explicit choice.
    func loadThisMac() throws -> MachineSourceResult {
        retireKeyRequest(); invalidateExport()
        guard let command = Self.machineSourceCommand() else { throw MachineSourceError.noCommand }
        let bytes = try Self.run(command.executable, command.arguments, limit: MachineCompanionSource.maximumBytes)
        try Task.checkCancellation()
        let source = try MachineCompanionSource(data: bytes)
        return try loadMachineSource(source)
    }
    func loadDesktopHandoff() throws -> MachineSourceResult {
        clear()
        defer { try? FileHandle.standardInput.close() }
        let bytes = try LocalHelperProcess.readInheritedInput()
        try Task.checkCancellation()
        let result = try loadMachineSource(MachineCompanionSource(data: bytes, requireRunningDesktop: true))
        try Task.checkCancellation()
        let digest = crypto.sha256(bytes).map { String(format: "%02x", $0) }.joined()
        // The desktop verifies this exact receipt before accepting the handoff;
        // an older companion that ignores the flag cannot silently substitute
        // its ordinary static-inventory startup path.
        try FileHandle.standardOutput.write(contentsOf: Data("FTDESKTOP3/\(digest)\n".utf8))
        return result
    }
    private func loadMachineSource(_ source: MachineCompanionSource) throws -> MachineSourceResult {
        let files = source.identityFiles, candidate = source.workspace
        let selected = try WorkspaceExportSelection(workspace: candidate, groupIDs: Set(candidate.groups.map(\.id)),
                                                    findings: source.findings)
        var loaded: [PrivateKeyRecord] = [], included: [String: WorkspaceExportSelection.IdentityDecision] = [:]
        var unresolved: [(credentialID: String, reason: String)] = []
        // A live desktop handoff supplies public references only. Its private
        // files are read only after Include Private Key + Use Key on This Mac.
        for identity in selected.identities where source.runtime == nil {
            try Task.checkCancellation()
            guard let credentialID = identity.credentialIDs.first(where: { files[$0] != nil }),
                  let path = files[credentialID], let url = Self.identityURL(path),
                  let reference = selected.workspace.credentials.first(where: { $0.id == credentialID }) else {
                if let first = identity.credentialIDs.first { unresolved.append((first, "no key file on this Mac")) }
                continue
            }
            do {
                switch try keyUnlocker.select(pem: SelectedLocalFile.read(url, limit: FTLimit.key), reference: reference) {
                case let .ready(key):
                    guard fingerprint(key) == identity.fingerprint else { throw OpenSSHSourceKeyError.identityMismatch }
                    loaded.append(key); included[identity.fingerprint] = .includePrivateKey
                case .passphraseRequired:
                    try? keyUnlocker.cancel()
                    unresolved.append((credentialID, "\(url.lastPathComponent) needs its passphrase"))
                }
            } catch is CancellationError { throw CancellationError() }
            catch {
                try? keyUnlocker.cancel()
                unresolved.append((credentialID, "\(url.lastPathComponent): \(error.localizedDescription)"))
            }
        }
        _ = try AirGapArchive(workspace: candidate, keys: loaded, using: crypto)
        try Task.checkCancellation()
        document = candidate; selection = selected; decisions = included; keys = loaded
        excludedMachineIDs = []
        sourceFindings = source.findings; identityReferences = files; sshSource = nil; invalidateExport()
        return .init(state: try state(), inventory: source.inventory, runtime: source.runtime, unresolved: unresolved)
    }
    func inspectSSHConfiguration(_ url: URL) throws -> [String] {
        retireKeyRequest(); invalidateExport()
        sshSource = nil
        let source = try SSHWorkspaceSource.loadUserConfiguration(url)
        guard !source.aliases.isEmpty else { throw FTError.unsupported("ssh.no_concrete_hosts") }
        try Task.checkCancellation()
        sshSource = (source, url)
        return source.aliases
    }
    func cancelSSHSelection() { retireKeyRequest(); sshSource = nil }
    static func identityURL(_ reference: String) -> URL? {
        guard !reference.contains("%"), !reference.contains("$"), !reference.contains("\0") else { return nil }
        if reference.hasPrefix("~/") {
            return FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(String(reference.dropFirst(2)))
        }
        guard reference.hasPrefix("/") else { return nil }
        return URL(fileURLWithPath: reference)
    }
    func loadSSHSelection(_ aliases: [String]) throws -> SelectionState {
        retireKeyRequest(); invalidateExport()
        guard let pending = sshSource else { throw FTError.stale }
        let publicIdentityFiles = try pending.source.loadPublicIdentityFiles(for: aliases)
        let publicCertificateFiles = try pending.source.loadPublicCertificateFiles(for: aliases)
        // Freeze the actual user/global trust-file selection for every chosen
        // destination and jump. Never pool unrelated aliases' trust or treat an
        // unreadable revocation source as an absent file.
        let knownHostFiles = try pending.source.loadKnownHostFiles(for: aliases)
        let support = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                                  appropriateFor: nil, create: true)
        let sourceID = crypto.sha256(Data(pending.url.standardizedFileURL.path.utf8)).map { String(format: "%02x", $0) }.joined()
        let storage = try FileWorkspacePersistence(directory: support.appendingPathComponent("FrankenTermExport/ssh-" + sourceID))
        let (candidate, selected): (SSHWorkspaceExport, WorkspaceExportSelection) = try storage.withExclusiveAccess {
            var registry = try storage.read(.catalog, limit: FTLimit.manifest).map {
                try JSONDecoder().decode(SSHWorkspaceRegistry.self, from: $0)
            } ?? SSHWorkspaceRegistry()
            let result = try pending.source.export(aliases: aliases, workspaceName: "SSH Machines", defaultUser: NSUserName(),
                publicIdentityFiles: publicIdentityFiles, publicCertificateFiles: publicCertificateFiles,
                knownHostFiles: knownHostFiles, registry: &registry, using: crypto)
            let selected = try WorkspaceExportSelection(workspace: result.workspace,
                groupIDs: [result.workspace.initialGroupID], findings: result.portability)
            _ = try selected.review(decisions: [:], loadedFingerprints: [])
            let bytes = try JSONEncoder().encode(registry)
            guard bytes.count <= FTLimit.manifest else { throw FTError.limit("ssh.registry") }
            try Task.checkCancellation()
            // Publish stable IDs durably before exposing the reviewed snapshot.
            try storage.write(bytes, to: .catalog)
            return (result, selected)
        }
        try Task.checkCancellation()
        document = candidate.workspace; selection = selected; sourceFindings = candidate.portability
        excludedMachineIDs = []
        identityReferences = candidate.identityReferences; sshSource = nil
        decisions = [:]; keys = []; invalidateExport()
        return try state()
    }
    func selectGroups(_ ids: Set<String>) throws -> SelectionState {
        retireKeyRequest(); invalidateExport()
        guard let document else { throw FTError.stale }
        let machines = Set(document.groups.filter { ids.contains($0.id) }.flatMap(\.machineOrder))
            .subtracting(excludedMachineIDs)
        let selected = try WorkspaceExportSelection(workspace: document, groupIDs: ids,
                                                    machineIDs: machines, findings: sourceFindings)
        return try applySelection(selected)
    }
    func selectMachines(_ ids: Set<String>) throws -> SelectionState {
        retireKeyRequest(); invalidateExport()
        guard let document, let selection else { throw FTError.stale }
        let selected = try WorkspaceExportSelection(workspace: document,
            groupIDs: Set(selection.workspace.groups.map(\.id)), machineIDs: ids, findings: sourceFindings)
        let available = Set(selected.availableMachines.map(\.id))
        excludedMachineIDs.subtract(available)
        excludedMachineIDs.formUnion(available.subtracting(ids))
        return try applySelection(selected)
    }
    private func applySelection(_ selected: WorkspaceExportSelection) throws -> SelectionState {
        let retained = Set(selected.identities.map(\.fingerprint))
        // A group or machine change releases unrelated private material and
        // its consent. Re-adding that machine cannot silently re-include a key.
        keys = keys.filter { retained.contains(fingerprint($0)) }
        decisions = decisions.filter { retained.contains($0.key) }
        selection = selected; invalidateExport()
        return try state()
    }
    func decide(_ value: WorkspaceExportSelection.IdentityDecision, fingerprint selectedFingerprint: String) throws -> SelectionState {
        retireKeyRequest(); invalidateExport()
        guard let selection, selection.identities.contains(where: { $0.fingerprint == selectedFingerprint }) else { throw FTError.stale }
        decisions[selectedFingerprint] = value
        if value != .includePrivateKey { keys.removeAll { fingerprint($0) == selectedFingerprint } }
        invalidateExport()
        return try state()
    }
    func loadKey(_ url: URL, credentialID: String, operationID: UUID) throws -> KeyLoadResult {
        retireKeyRequest(); invalidateExport()
        sourceKeyOwner = operationID
        guard let selection,
              let reference = selection.workspace.credentials.first(where: { $0.id == credentialID }) else { throw FTError.stale }
        var sourceBytes = try SelectedLocalFile.read(url, limit: FTLimit.key)
        defer { sourceBytes.resetBytes(in: 0..<sourceBytes.count) }
        let selected = try keyUnlocker.select(pem: sourceBytes, reference: reference)
        do { try Task.checkCancellation() }
        catch { retireKeyRequest(); throw error }
        switch selected {
        case let .ready(key):
            return .loaded(try installKey(key, requestID: nil))
        case let .passphraseRequired(request): return .passphraseRequired(request)
        }
    }
    func loadConfiguredKey(_ file: WorkspaceExportSelection.ConfiguredIdentityFile,
                           operationID: UUID) throws -> KeyLoadResult {
        // A delayed UI action cannot change its file, identity or inclusion
        // decision after a group/source change. The normal source unlocker then
        // verifies the actual public header and private key before admission.
        guard let selection, decisions[file.fingerprint] == .includePrivateKey,
              selection.configuredIdentityFile(for: file.fingerprint, fileReferences: identityReferences) == file,
              let hint = Self.identityURL(file.fileReference) else { throw FTError.stale }
        let url = hint.pathExtension == "pub" ? hint.deletingPathExtension() : hint
        return try loadKey(url, credentialID: file.credentialID, operationID: operationID)
    }
    func unlockKey(operationID: UUID, requestID: UUID, passphrase: Data) throws -> SelectionState {
        // This actor serializes the bounded synchronous KDF. No passphrase or
        // decrypted source bytes are persisted or sent to another process.
        guard sourceKeyOwner == operationID else { throw FTError.stale }
        let key = try keyUnlocker.unlock(requestID: requestID, passphrase: passphrase)
        try Task.checkCancellation()
        return try installKey(key, requestID: requestID)
    }
    func pendingKeyRequest(operationID: UUID) -> SourceKeyUnlockRequest? {
        sourceKeyOwner == operationID ? keyUnlocker.request : nil
    }
    func cancelKeyUnlock(operationID: UUID, requestID: UUID? = nil) {
        guard sourceKeyOwner == operationID else { return }
        if let requestID, keyUnlocker.request?.id != requestID, pendingKeyInstallation?.requestID != requestID { return }
        retireKeyRequest()
    }
    func acknowledgeKey(operationID: UUID) {
        guard sourceKeyOwner == operationID, pendingKeyInstallation != nil else { return }
        // MainActor has already accepted and published this public selection
        // state. Finish its ownership handoff even if its task is later canceled.
        pendingKeyInstallation = nil
        try? keyUnlocker.cancel(); sourceKeyOwner = nil
    }
    private func installKey(_ key: PrivateKeyRecord, requestID: UUID?) throws -> SelectionState {
        guard let document, let selection,
              let reference = selection.workspace.credentials.first(where: { $0.id == key.credentialID }) else { throw FTError.stale }
        let selectedFingerprint = fingerprint(key)
        guard reference.fingerprint == selectedFingerprint else { throw OpenSSHSourceKeyError.identityMismatch }
        var next = keys.filter { fingerprint($0) != selectedFingerprint }
        // A single material record resolves all aliases of the same public identity.
        next.append(key)
        _ = try AirGapArchive(workspace: document, keys: next, using: crypto)
        try Task.checkCancellation()
        // Keep the old state until the foreground UI accepts this exact result.
        // Cancellation before that handoff restores only this operation's keys.
        pendingKeyInstallation = .init(requestID: requestID, previousKeys: keys, previousDecisions: decisions)
        keys = next; decisions[selectedFingerprint] = .includePrivateKey; invalidateExport()
        return try state()
    }
    func scanRecipientImage(_ url: URL) throws -> RecipientRequest {
        invalidateExport()
        let bytes = try SelectedLocalFile.read(url, limit: 8 * 1024 * 1024)
        return try autoreleasepool {
            guard let source = CGImageSourceCreateWithData(bytes as CFData, [kCGImageSourceShouldCache as String: false] as CFDictionary),
                  CGImageSourceGetCount(source) == 1,
                  let type = CGImageSourceGetType(source),
                  [UTType.png.identifier, UTType.jpeg.identifier, UTType.heic.identifier].contains(type as String),
                  let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [String: Any],
                  let width = properties[kCGImagePropertyPixelWidth as String] as? Int,
                  let height = properties[kCGImagePropertyPixelHeight as String] as? Int,
                  width > 0, height > 0, width <= 8192, height <= 8192,
                  width <= 16_777_216 / height else { throw FTError.limit("recipient.image") }
            // Downsample before Vision sees pixels; never decode an unbounded
            // image or interpret a file/URL as a command or recipient request.
            let options: [String: Any] = [kCGImageSourceCreateThumbnailFromImageAlways as String: true,
                                          kCGImageSourceCreateThumbnailWithTransform as String: true,
                                          kCGImageSourceThumbnailMaxPixelSize as String: 2048,
                                          kCGImageSourceShouldCacheImmediately as String: false]
            guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary),
                  image.width <= 2048, image.height <= 2048 else { throw FTError.malformed("recipient.image") }
            try Task.checkCancellation()
            let scan = VNDetectBarcodesRequest(); scan.symbologies = [.qr]
            try VNImageRequestHandler(cgImage: image, options: [:]).perform([scan])
            guard let observations = scan.results, observations.count == 1,
                  let text = observations[0].payloadStringValue,
                  text.hasPrefix("FTREQ1:"), text.utf8.count <= FTLimit.text else {
                throw FTError.malformed("recipient.image_qr")
            }
            try Task.checkCancellation()
            return try RecipientRequest(text: text)
        }
    }
    func prepare(protection: ExportProtection, recipient: RecipientRequest?, confirmedRequestDigest: Data?) throws -> Prepared {
        retireKeyRequest(); invalidateExport()
        try Task.checkCancellation()
        guard let selection else { throw FTError.stale }
        // Each new seal is a distinct immutable export, including when a user
        // changes selected keys or prepares the same source document again.
        // Playback and file/animation saves continue reusing this one envelope.
        let archive = try selection.archive(decisions: decisions, keys: keys, bundleID: UUID().uuidString, using: crypto)
        let preparedTransfer: OpticalExportSession
        let recipientComparison: String?
        switch protection {
        case .recipient:
            guard let recipient, let confirmedRequestDigest else { throw FTError.malformed("export.recipient_confirmation") }
            let envelope = try FTAG.seal(archive, recipient: recipient, confirmedRequestDigest: confirmedRequestDigest, using: crypto)
            preparedTransfer = try OpticalExportSession(envelope: envelope, using: crypto)
            recipientComparison = recipient.comparison(using: crypto)
        case .code:
            guard recipient == nil, confirmedRequestDigest == nil else { throw FTError.malformed("export.protection_selection") }
            let candidate = try FTAG.seal(archive, using: crypto)
            preparedTransfer = try OpticalExportSession(export: candidate, using: crypto)
            recipientComparison = nil
        }
        try Task.checkCancellation()
        transfer = preparedTransfer
        return metadata(preparedTransfer, recipientComparison: recipientComparison, isReplay: false)
    }
    func openEncryptedTransfer(_ url: URL) throws -> Prepared {
        let bytes = try SelectedLocalFile.read(url, limit: FTLimit.envelope)
        let preparedTransfer = try OpticalExportSession(envelope: bytes, using: crypto)
        try Task.checkCancellation()
        clear()
        transfer = preparedTransfer
        return metadata(preparedTransfer, recipientComparison: nil, isReplay: true)
    }
    private func metadata(_ value: OpticalExportSession, recipientComparison: String?, isReplay: Bool) -> Prepared {
        .init(id: value.id, comparison: FTAG.comparison(value.envelope, using: crypto), pages: value.identity.pageCount,
              bytes: value.envelope.count, protection: value.mode == .code ? .code : .recipient,
              recipientComparison: recipientComparison, hasTransferCode: value.hasTransferCode, isReplay: isReplay)
    }
    func beginPlayback(exportID: UUID, rate: Int, repairPage: Int?) throws -> OpticalExportSession.Playback {
        guard var active = transfer, active.id == exportID else { throw FTError.stale }
        let lease = try active.beginPlayback(symbolsPerSecond: rate, repairPage: repairPage)
        transfer = active
        return lease
    }
    func next(exportID: UUID, playback: OpticalExportSession.Playback, rate: Int, repairPage: Int?) throws -> OpticalExportSession.Frame {
        guard var active = transfer, active.id == exportID else { throw FTError.stale }
        try active.updatePlayback(playback, symbolsPerSecond: rate, repairPage: repairPage)
        let frame = try active.next(playback, using: crypto)
        transfer = active
        return frame
    }
    func pausePlayback(exportID: UUID, playback: OpticalExportSession.Playback) {
        guard var active = transfer, active.id == exportID else { return }
        active.pause(playback); transfer = active
    }
    func revealTransferCode(exportID: UUID) throws -> (code: String, frame: String) {
        guard var active = transfer, active.id == exportID else { throw FTError.stale }
        let code = try active.revealTransferCode(using: crypto)
        transfer = active
        return (code, TransferCodeFrame.text(envelopeDigest: active.identity.envelopeDigest, code: code))
    }
    func encryptedEnvelope(exportID: UUID) throws -> Data {
        guard let transfer, transfer.id == exportID else { throw FTError.stale }
        return transfer.envelope
    }
    func saveEncryptedFile(exportID: UUID, to url: URL) throws {
        try save(encryptedEnvelope(exportID: exportID), to: url)
    }
    func savePublicWorkspace(to url: URL) throws -> String {
        guard let selection else { throw FTError.stale }
        let publicDocument = try selection.publicDocument(bundleID: UUID().uuidString)
        try Task.checkCancellation()
        try save(publicDocument.raw, to: url)
        return try PublicWorkspaceIdentity.comparison(data: publicDocument.raw, using: crypto)
    }
    private func save(_ bytes: Data, to url: URL) throws {
        guard url.isFileURL else { throw FTError.stale }
        let access = url.startAccessingSecurityScopedResource()
        defer { if access { url.stopAccessingSecurityScopedResource() } }
        try LocalExportFile.write(bytes, to: url)
    }
    func clear() {
        retireKeyRequest(); document = nil; selection = nil; decisions = [:]; keys = []
        excludedMachineIDs = []
        sourceFindings = []; identityReferences = [:]; sshSource = nil; invalidateExport()
    }
}

private actor MacPhoneKeyEnrollmentWorker {
    func loadRequest(_ url: URL) throws -> WorkspaceKeyEnrollmentRequest {
        try WorkspaceKeyEnrollmentRequest(data: SelectedLocalFile.read(url, limit: WorkspaceKeyEnrollmentRequest.maximumBytes))
    }

    private func storage(request: WorkspaceKeyEnrollmentRequest,
                         target: WorkspaceKeyEnrollmentTarget) throws -> FileWorkspacePersistence {
        guard let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first else {
            throw FTError.storage
        }
        let directory = support.appendingPathComponent("FrankenTerm", isDirectory: true)
            .appendingPathComponent("PhoneKeyEnrollment", isDirectory: true)
            .appendingPathComponent(request.id, isDirectory: true)
            .appendingPathComponent(target.id, isDirectory: true)
        return try FileWorkspacePersistence(directory: directory)
    }

    private func journal(in storage: FileWorkspacePersistence, request: WorkspaceKeyEnrollmentRequest,
                         target: WorkspaceKeyEnrollmentTarget) throws -> WorkspaceKeyEnrollmentJournal {
        guard let bytes = try storage.read(.catalog, limit: WorkspaceKeyEnrollmentJournal.maximumBytes) else {
            return try WorkspaceKeyEnrollmentJournal(request: request, targets: [target])
        }
        let saved = try WorkspaceKeyEnrollmentJournal(data: bytes)
        guard try saved.request.encoded() == request.encoded(), saved.targets == [target] else {
            throw FTError.malformed("enrollment.saved_request")
        }
        return saved
    }

    func readJournal(request: WorkspaceKeyEnrollmentRequest,
                     target: WorkspaceKeyEnrollmentTarget) throws -> WorkspaceKeyEnrollmentJournal {
        try Task.checkCancellation()
        let persistence = try storage(request: request, target: target)
        return try persistence.withExclusiveAccess {
            try journal(in: persistence, request: request, target: target)
        }
    }

    func perform(request: WorkspaceKeyEnrollmentRequest, target: WorkspaceKeyEnrollmentTarget,
                 action: WorkspaceKeyEnrollmentAction) throws -> WorkspaceKeyEnrollmentJournal {
        let persistence = try storage(request: request, target: target)
        return try persistence.withExclusiveAccess {
            var saved = try journal(in: persistence, request: request, target: target)
            try Task.checkCancellation()
            if action != .inspect {
                try saved.beginMutation(targetID: target.id, action: action)
                // Both installation and removal publish their intent and
                // uncertainty durably before any remote mutation can begin.
                try persistence.write(saved.encoded(), to: .catalog)
            }
            let result: WorkspaceKeyEnrollmentResult
            do {
                result = try WorkspaceKeyEnrollmentSSH.perform(request: request, target: target, action: action,
                    isCancelled: { Task<Never, Never>.isCancelled })
            } catch {
                result = try WorkspaceKeyEnrollmentResult(operationID: request.operationID(for: target), target: target,
                    state: action != .inspect ? .uncertain : .blocked,
                    detail: action != .inspect
                        ? "The connection did not confirm the outcome. Reconcile this account before another change."
                        : "The account could not be inspected through this Mac's trusted SSH access.")
            }
            try saved.record(result, action: action)
            // A save error stops the batch, including after SSH succeeded.
            try persistence.write(saved.encoded(), to: .catalog)
            guard saved.result(for: target.id) != nil else { throw FTError.storage }
            return saved
        }
    }

    func saveResults(_ bytes: Data, to url: URL) throws {
        guard bytes.count <= WorkspaceKeyEnrollmentJournal.maximumBytes else { throw FTError.limit("enrollment.results") }
        let access = url.startAccessingSecurityScopedResource()
        defer { if access { url.stopAccessingSecurityScopedResource() } }
        try LocalExportFile.write(bytes, to: url)
    }
}

private struct MacPhoneKeyEnrollmentRow: Identifiable {
    let id: String
    var machines: [String]
    let target: WorkspaceKeyEnrollmentTarget?
    let unavailableReason: String?
    var journal: WorkspaceKeyEnrollmentJournal?
    var result: WorkspaceKeyEnrollmentResult? {
        guard let target else { return nil }
        return journal?.result(for: target.id)
    }
}

@MainActor
@Observable
private final class MacPhoneKeyEnrollmentModel {
    private(set) var request: WorkspaceKeyEnrollmentRequest?
    private(set) var rows: [MacPhoneKeyEnrollmentRow] = []
    private(set) var selectedTargetIDs = Set<String>()
    private(set) var confirmedFingerprint = false
    private(set) var busy = false
    private(set) var message: String?
    @ObservationIgnored private let worker = MacPhoneKeyEnrollmentWorker()
    @ObservationIgnored private var work: Task<Void, Never>?
    @ObservationIgnored private var workID: UUID?
    @ObservationIgnored private var windowOpen = false
    @ObservationIgnored private var active = false
    @ObservationIgnored private var sceneActive = false
    @ObservationIgnored private var sessionActive = true
    @ObservationIgnored private var screensAwake = true
    @ObservationIgnored private var sourceDigest: Data?
    @ObservationIgnored private var reviewDigest: Data?

    private struct Approval: Equatable {
        let request: WorkspaceKeyEnrollmentRequest
        let sourceDigest: Data
        let targetIDs: [String]
        let action: WorkspaceKeyEnrollmentAction
    }

    var canAct: Bool { approval(for: .inspect) != nil }
    var canSaveResults: Bool {
        !busy && request != nil && reviewDigest != nil && reviewDigest == sourceDigest
            && rows.contains { $0.target != nil }
    }
    var eligibleCount: Int { rows.filter { $0.target != nil }.count }

    func open(isActive: Bool) { windowOpen = true; setActive(isActive) }
    func setActive(_ value: Bool) {
        sceneActive = value; updateActivity()
    }
    func setSessionActive(_ value: Bool) { sessionActive = value; updateActivity() }
    func setScreensAwake(_ value: Bool) { screensAwake = value; updateActivity() }
    private func updateActivity() {
        // A scene activation cannot override a still-locked login session or
        // sleeping screens, and a screen wake cannot override a closed view.
        active = windowOpen && sceneActive && sessionActive && screensAwake
        if !active { confirmedFingerprint = false; work?.cancel() }
    }
    func close() { windowOpen = false; sceneActive = false; updateActivity() }
    func closeAndWait() async {
        close()
        let pending = work
        await pending?.value
    }
    func cancel() { work?.cancel() }

    static func sourceWitness(_ document: WorkspaceDocument?, machines: [WorkspaceDocument.Machine]) -> Data? {
        guard let document else { return nil }
        var identity = document.raw
        identity.append(0)
        for machine in machines { identity.append(contentsOf: machine.id.utf8); identity.append(0) }
        return AppleCryptography().sha256(identity)
    }

    func sourceChanged(_ document: WorkspaceDocument?, machines: [WorkspaceDocument.Machine]) {
        let next = Self.sourceWitness(document, machines: machines)
        guard next != sourceDigest else { return }
        sourceDigest = next; reviewDigest = nil; confirmedFingerprint = false; selectedTargetIDs = []
        work?.cancel()
        if !busy { rows = [] }
        if request != nil { message = "The selected machines changed. Review the current accounts before contacting servers." }
    }

    func confirmFingerprint(_ value: Bool) {
        guard !busy, active, request != nil else { return }
        confirmedFingerprint = value
    }
    func select(_ targetID: String, included: Bool) {
        guard !busy, rows.contains(where: { $0.target?.id == targetID }) else { return }
        if included { selectedTargetIDs.insert(targetID) } else { selectedTargetIDs.remove(targetID) }
    }
    func selectAll() { guard !busy else { return }; selectedTargetIDs = Set(rows.compactMap { $0.target?.id }) }
    func selectNone() { guard !busy else { return }; selectedTargetIDs = [] }

    func chooseRequest() {
        guard !busy, windowOpen, active else { return }
        let panel = NSOpenPanel()
        panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.json, .data]
        panel.title = "Open Phone Key Enrollment Request"
        panel.message = "Choose the original public .ftkeyrequest and verify its fingerprint against the phone or a trusted saved record. Removing an enrollment requires the request used for that installation."
        guard panel.runModal() == .OK, let url = panel.url, windowOpen, active, !busy else { return }
        // Choosing another file retires the old candidate even if the new file
        // fails validation. An error must not leave the old key approved.
        request = nil; rows = []; selectedTargetIDs = []; reviewDigest = nil; confirmedFingerprint = false
        perform(failure: "This file is not a valid public phone-key enrollment request.") { [self] token in
            let loaded = try await worker.loadRequest(url)
            try requireCurrent(token)
            request = loaded; rows = []; selectedTargetIDs = []; reviewDigest = nil; confirmedFingerprint = false
            message = "Public key loaded. Review the selected accounts and verify the fingerprint against the phone or a trusted saved record."
        }
    }

    func review(document: WorkspaceDocument, machines: [WorkspaceDocument.Machine]) {
        guard !busy, windowOpen, active, let request,
              sourceDigest == Self.sourceWitness(document, machines: machines),
              !machines.isEmpty, machines.count <= WorkspaceKeyEnrollmentJournal.maximumTargets else { return }
        var candidates: [MacPhoneKeyEnrollmentRow] = []
        var existing: [String: Int] = [:]
        for machine in machines {
            do {
                guard let route = document.routes.first(where: { $0.id == machine.preferredRouteID }) else {
                    throw WorkspaceKeyEnrollmentSSHError.route
                }
                let target = try WorkspaceKeyEnrollmentSSH.target(for: route, in: document)
                if let index = existing[target.id] { candidates[index].machines.append(machine.name) }
                else {
                    existing[target.id] = candidates.count
                    candidates.append(.init(id: target.id, machines: [machine.name], target: target,
                        unavailableReason: nil, journal: nil))
                }
            } catch {
                let reason = (error as? WorkspaceKeyEnrollmentSSHError)?.localizedDescription
                    ?? "This route cannot preserve its current policy during direct phone-key enrollment."
                candidates.append(.init(id: "unavailable:" + machine.id, machines: [machine.name],
                    target: nil, unavailableReason: reason, journal: nil))
            }
        }
        rows = candidates; selectedTargetIDs = []; confirmedFingerprint = false; reviewDigest = nil
        perform(failure: "Saved enrollment receipts could not be verified. No server was contacted. Check local storage before continuing.") { [self] token in
            for target in candidates.compactMap({ $0.target }) {
                try requireCurrent(token)
                let restored = try await worker.readJournal(request: request, target: target)
                try requireCurrent(token)
                if let index = rows.firstIndex(where: { $0.target?.id == target.id }) { rows[index].journal = restored }
            }
            try requireCurrent(token)
            guard sourceDigest == Self.sourceWitness(document, machines: machines) else { throw FTError.stale }
            reviewDigest = sourceDigest
            message = "Saved account receipts loaded for review. Select accounts and verify the phone-key fingerprint before contacting servers."
        }
    }

    private func approval(for action: WorkspaceKeyEnrollmentAction) -> Approval? {
        guard !busy, windowOpen, active, confirmedFingerprint, let request,
              let sourceDigest, sourceDigest == reviewDigest, !selectedTargetIDs.isEmpty else { return nil }
        let eligible = Set(rows.compactMap { $0.target?.id })
        guard selectedTargetIDs.isSubset(of: eligible) else { return nil }
        return .init(request: request, sourceDigest: sourceDigest, targetIDs: selectedTargetIDs.sorted(), action: action)
    }

    func authorize() { confirmMutation(.install) }

    func removeEnrolledAccess() { confirmMutation(.remove) }

    private func confirmMutation(_ action: WorkspaceKeyEnrollmentAction) {
        guard action != .inspect, let reviewed = approval(for: action) else { return }
        let accounts = rows.compactMap { row -> String? in
            guard let target = row.target, reviewed.targetIDs.contains(target.id) else { return nil }
            return "\(target.user)@\(target.host):\(target.port)"
        }
        let alert = NSAlert()
        alert.alertStyle = action == .remove ? .critical : .informational
        alert.messageText = action == .remove
            ? "Remove this enrollment's access on \(accounts.count) accounts?"
            : "Authorize this phone key on \(accounts.count) accounts?"
        let explanation = action == .remove
            ? "This Mac will disable only the unrestricted public-key entry installed by this exact enrollment, preserving it as a comment for audit. Independently installed or restricted matching entries remain and will be reported. Use the original enrollment request; a newly exported request does not identify the earlier installation. Existing SSH sessions and their remote processes are not terminated."
            : "This Mac will sign in through its existing trusted SSH access and add only this public key to each account's standard authorized_keys file. Existing restrictions stay intact. Verify a connection from the phone after enrollment."
        alert.informativeText = reviewed.request.key.fingerprint + "\nEnrollment request: " + reviewed.request.id + "\n\n"
            + accounts.prefix(12).joined(separator: "\n")
            + (accounts.count > 12 ? "\n…and \(accounts.count - 12) other reviewed accounts." : "")
            + "\n\n" + explanation
        alert.addButton(withTitle: action == .remove ? "Remove Enrolled Access" : "Authorize Listed Accounts")
        alert.addButton(withTitle: "Cancel")
        // The action is part of the immutable review. An authorization prompt
        // can never approve removal, or a later source/selection/request.
        guard alert.runModal() == .alertFirstButtonReturn, approval(for: action) == reviewed else { return }
        run(approval: reviewed)
    }

    func reconcile() {
        guard let reviewed = approval(for: .inspect) else { return }
        run(approval: reviewed)
    }

    private func run(approval reviewed: Approval) {
        guard approval(for: reviewed.action) == reviewed else { return }
        let action = reviewed.action
        let targets = rows.compactMap { row -> WorkspaceKeyEnrollmentTarget? in
            guard let target = row.target, reviewed.targetIDs.contains(target.id) else { return nil }
            return target
        }
        guard targets.count == reviewed.targetIDs.count else { return }
        perform(failure: "The account batch stopped because its durable receipt could not be saved or verified. Reconcile any uncertain accounts before another change.") { [self] token in
            for target in targets {
                try requireCurrent(token)
                guard request == reviewed.request, sourceDigest == reviewed.sourceDigest,
                      reviewDigest == reviewed.sourceDigest else { throw FTError.stale }
                if action != .inspect, let index = rows.firstIndex(where: { $0.target?.id == target.id }) {
                    var pending = try rows[index].journal
                        ?? WorkspaceKeyEnrollmentJournal(request: reviewed.request, targets: [target])
                    try pending.beginMutation(targetID: target.id, action: action)
                    rows[index].journal = pending
                }
                let updated = try await worker.perform(request: reviewed.request, target: target, action: action)
                try requireCurrent(token)
                if let index = rows.firstIndex(where: { $0.target?.id == target.id }) { rows[index].journal = updated }
            }
            let selectedResults = rows.filter { reviewed.targetIDs.contains($0.id) }.compactMap { $0.result }
            let absent = selectedResults.filter { $0.state == .absent || $0.state == .removed }.count
            let present = selectedResults.filter {
                $0.state == .installed || $0.state == .alreadyPresent || $0.state == .restricted || $0.state == .stillAuthorized
            }.count
            let unknown = selectedResults.filter { $0.state == .uncertain }.count
            let blocked = selectedResults.filter { $0.state == .blocked }.count
            let operation: String
            switch action {
            case .install: operation = "Authorization"
            case .remove: operation = "Scoped removal"
            case .inspect: operation = "Read-only reconciliation"
            }
            message = "\(operation) results saved for \(targets.count) accounts: \(absent) have no live matching key entry, \(present) have matching entries, \(blocked) need attention, and \(unknown) have unknown outcomes. Review each account's receipt."
                + (unknown > 0 ? " Reconcile unknown outcomes before another change." : "")
                + (action == .install
                    ? " Verify server acceptance with a connection from the phone."
                    : " These are file observations. Existing SSH sessions and their remote processes are not terminated.")
        }
    }

    func saveResults() {
        guard canSaveResults, windowOpen, active, let request else { return }
        let targets = rows.compactMap { $0.target }; guard !targets.isEmpty else { return }
        do {
            let reviewedSource = reviewDigest
            var report = try WorkspaceKeyEnrollmentJournal(request: request, targets: targets)
            for row in rows {
                guard let target = row.target, let journal = row.journal else { continue }
                guard journal.targets == [target] else { throw FTError.stale }
                // Copy exact receipts and their intent. Replaying begin/record
                // could turn a later blocked inspection into new uncertainty.
                try report.mergeResults(from: journal)
            }
            let bytes = try report.encoded()
            let panel = NSSavePanel(); panel.allowedContentTypes = [.json]
            panel.nameFieldStringValue = "FrankenTerm-Enrollment-\(request.id).json"
            panel.message = "Save public account results, operation IDs and the last requested change. This report includes host names, accounts and public-key fingerprints, without private keys."
            guard panel.runModal() == .OK, let url = panel.url, windowOpen, active, canSaveResults,
                  self.request == request, sourceDigest == reviewedSource, reviewDigest == reviewedSource else { return }
            perform(failure: "Enrollment results could not be saved. Choose another local filename.") { [self] token in
                try await worker.saveResults(bytes, to: url); try requireCurrent(token)
                message = "Public enrollment results saved."
            }
        } catch { message = "Enrollment results could not be prepared for saving." }
    }

    private func requireCurrent(_ token: UUID) throws {
        guard !Task.isCancelled, windowOpen, active, workID == token else { throw CancellationError() }
    }

    private func perform(failure: String, _ body: @escaping @MainActor (UUID) async throws -> Void) {
        guard !busy, windowOpen, active else { return }
        let token = UUID(); workID = token; busy = true; message = nil
        work = Task {
            defer { if workID == token { workID = nil; work = nil; busy = false } }
            do { try requireCurrent(token); try await body(token) }
            catch {
                if windowOpen, workID == token {
                    if error is CancellationError {
                        message = reviewDigest == nil
                            ? "Review the current accounts before contacting servers."
                            : "Stopped. Saved receipts remain available; reconcile any account whose outcome is unknown."
                    } else { message = failure }
                }
            }
        }
    }
}

@MainActor
@Observable
private final class MacExportModel {
    var document: WorkspaceDocument?
    var selection: WorkspaceExportSelection?
    var identityDecisions: [String: WorkspaceExportSelection.IdentityDecision] = [:]
    var exportReview: WorkspaceExportSelection.Review?
    var includedFingerprints = Set<String>()
    var identityReferences: [String: String] = [:]
    var sshAliases: [String] = []
    var selectedSSHAliases = Set<String>()
    var sshSearch = ""
    var sourceSheet: ExportSourceSheet?
    var keyUnlockRequest: SourceKeyUnlockRequest?
    var prepared: MacExportEngine.Prepared?
    private(set) var revealedTransferCode: String?
    private(set) var revealedCodeSymbol: QRCodeSymbol?
    var symbol: QRCodeSymbol?
    var message: String?
    var busy = false
    var playing = false
    var emitted = 0
    private(set) var rate = 5
    private(set) var repairPage = -1
    var approveSelectedKeys = false
    private(set) var protection = ExportProtection.code
    private(set) var recipientRequest: RecipientRequest?
    private(set) var recipientComparison: String?
    private(set) var confirmedRecipientDigest: Data?
    private(set) var scanningRecipient = false
    private(set) var requestingCameraAccess = false
    private(set) var recipientScanID = UUID()
    @ObservationIgnored private let engine = MacExportEngine()
    @ObservationIgnored private let renderer = NativeOpticalPresentation()
    @ObservationIgnored private let videoRenderer = NativeOpticalVideo()
    @ObservationIgnored private var playback: Task<Void, Never>?
    @ObservationIgnored private var work: Task<Void, Never>?
    @ObservationIgnored private var workOperationID: UUID?
    @ObservationIgnored private var closingCleanup = false
    @ObservationIgnored private var generation = UUID()
    @ObservationIgnored private var awake: NSObjectProtocol?
    @ObservationIgnored private var windowOpen = false
    @ObservationIgnored private var foreground = false
    @ObservationIgnored private var cameraPermission: Task<Void, Never>?
    @ObservationIgnored private var sourceKeyOperation: UUID?
    @ObservationIgnored private var sshSelectionID: UUID?
    @ObservationIgnored private var loadedAtLaunch = false
    @ObservationIgnored private var playWhenPrepared = false
    @ObservationIgnored private var playbackSettings = UUID()
    @ObservationIgnored private var codeRevealEpoch = UUID()
    @ObservationIgnored private var sessionActive = true
    @ObservationIgnored private var screensAwake = true

    var selectedGroupIDs: Set<String> { Set(selection?.workspace.groups.map(\.id) ?? []) }
    var selectedMachineIDs: Set<String> { Set(selection?.workspace.machines.map(\.id) ?? []) }
    func canDeselectGroup(_ id: String) -> Bool {
        guard let document else { return false }
        return document.groups.contains { group in
            group.id != id && selectedGroupIDs.contains(group.id)
                && group.machineOrder.contains(where: selectedMachineIDs.contains)
        }
    }
    var recipientConfirmed: Bool {
        guard let recipientRequest, let confirmedRecipientDigest else { return false }
        return AppleCryptography().sha256(recipientRequest.encoded) == confirmedRecipientDigest
    }
    var destinationReady: Bool {
        protection == .code || (recipientRequest != nil && recipientConfirmed)
    }
    var filteredSSHAliases: [String] {
        sshSearch.isEmpty ? sshAliases : sshAliases.filter { $0.localizedCaseInsensitiveContains(sshSearch) }
    }
    private func apply(_ result: MacExportEngine.SelectionState) {
        document = result.source; selection = result.selection; identityDecisions = result.decisions
        includedFingerprints = result.loadedFingerprints; exportReview = result.review
        identityReferences = result.identityReferences
        prepared = nil; approveSelectedKeys = false; emitted = 0; repairPage = -1
    }
    private func invalidatePrepared() {
        pause(); prepared = nil; approveSelectedKeys = false; emitted = 0; repairPage = -1
        playWhenPrepared = false
    }
    private func retireSourceKeyPrompt() {
        sourceKeyOperation = nil; keyUnlockRequest = nil
        if case .some(.keyUnlock(_)) = sourceSheet { sourceSheet = nil }
    }
    private func retireSSHSelection() {
        sshSelectionID = nil; sshAliases = []; selectedSSHAliases = []; sshSearch = ""
        if case .some(.sshHosts(_)) = sourceSheet { sourceSheet = nil }
    }
    func sourceSheetDismissed(_ sheet: ExportSourceSheet) {
        // The content retains its own ID after SwiftUI clears the presentation.
        // A late dismissal cannot cancel a newly selected file or source.
        switch sheet {
        case let .sshHosts(id): dismissSSHSelection(id: id)
        case let .keyUnlock(request): cancelKeyUnlock(requestID: request.id)
        case let .recipientCamera(id): cancelRecipientScan(scanID: id)
        }
    }

    func selectProtection(_ value: ExportProtection) {
        guard !busy, sourceSheet == nil, sourceKeyOperation == nil, value != protection else { return }
        cancelRecipientScan(); invalidatePrepared()
        protection = value; recipientRequest = nil; recipientComparison = nil; confirmedRecipientDigest = nil
        perform { [self] _ in await engine.invalidateExport() }
    }
    func confirmRecipient(_ value: Bool, requestDigest: Data) {
        guard !busy, foreground, NSApplication.shared.isActive, sourceSheet == nil,
              sourceKeyOperation == nil, protection == .recipient, let recipientRequest,
              AppleCryptography().sha256(recipientRequest.encoded) == requestDigest else { return }
        // The control captures the exact request displayed when it was rendered.
        // A queued callback from an old control cannot approve a different phone.
        let digest = value ? requestDigest : nil
        guard digest != confirmedRecipientDigest else { return }
        invalidatePrepared(); confirmedRecipientDigest = digest
        perform { [self] _ in await engine.invalidateExport() }
    }
    private func resetRecipient() {
        cancelRecipientScan(); invalidatePrepared()
        recipientRequest = nil; recipientComparison = nil; confirmedRecipientDigest = nil
    }
    private func acceptRecipient(_ request: RecipientRequest) {
        recipientRequest = request; recipientComparison = request.comparison(using: AppleCryptography())
        confirmedRecipientDigest = nil; approveSelectedKeys = false
        message = "Compare the recipient value directly with the iPhone before approving this destination."
    }
    private func presentRecipientCamera(scanID: UUID) {
        guard foreground, NSApplication.shared.isActive, protection == .recipient, recipientScanID == scanID,
              sourceSheet == nil, sourceKeyOperation == nil, sshSelectionID == nil else { return }
        scanningRecipient = true; sourceSheet = .recipientCamera(scanID)
    }
    func scanRecipientWithCamera() {
        guard !busy, foreground, sourceSheet == nil, sourceKeyOperation == nil,
              protection == .recipient, !requestingCameraAccess else { return }
        resetRecipient()
        let token = recipientScanID
        perform { [self] operation in
            await engine.invalidateExport()
            try requireCurrentWork(operation)
            guard foreground, protection == .recipient, recipientScanID == token else { return }
            let status = AVCaptureDevice.authorizationStatus(for: .video)
            if status == .authorized { presentRecipientCamera(scanID: token); return }
            guard status == .notDetermined else {
                message = "Camera access is denied or restricted. Allow FrankenTerm Export in System Settings, or scan a local image of the phone’s recipient QR."
                return
            }
            requestingCameraAccess = true
            cameraPermission = Task { [weak self] in
                let granted = await AVCaptureDevice.requestAccess(for: .video)
                guard let self, self.recipientScanID == token else { return }
                self.requestingCameraAccess = false; self.cameraPermission = nil
                guard !Task.isCancelled, self.foreground, NSApplication.shared.isActive,
                      self.protection == .recipient else { return }
                if granted { self.presentRecipientCamera(scanID: token) }
                else { self.message = "Camera access was not granted. You can scan a local image of the recipient QR, or use a generated transfer code." }
            }
        }
    }
    func cancelRecipientScan(scanID: UUID? = nil) {
        if let scanID, scanID != recipientScanID { return }
        recipientScanID = UUID(); scanningRecipient = false; requestingCameraAccess = false
        cameraPermission?.cancel(); cameraPermission = nil
        if case .some(.recipientCamera(_)) = sourceSheet { sourceSheet = nil }
    }
    func receiveRecipient(_ request: RecipientRequest, scanID: UUID) {
        guard foreground, NSApplication.shared.isActive, !busy, protection == .recipient, scanningRecipient,
              sourceSheet?.id == scanID, recipientScanID == scanID else { return }
        cancelRecipientScan(scanID: scanID)
        acceptRecipient(request)
    }
    func recipientCameraFailed(_ error: RecipientRequestCameraError, scanID: UUID) {
        guard scanningRecipient, sourceSheet?.id == scanID, recipientScanID == scanID else { return }
        cancelRecipientScan(scanID: scanID); message = error.localizedDescription
    }
    func scanRecipientImage() {
        guard !busy, sourceSheet == nil, sourceKeyOperation == nil, protection == .recipient else { return }
        // A delayed camera-permission callback must not present over a file panel.
        cancelRecipientScan()
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.png, .jpeg, .heic]
        panel.message = "Choose a local image containing only the iPhone’s FrankenTerm recipient-request QR. You must compare its recipient value with the phone before exporting."
        guard panel.runModal() == .OK, let url = panel.url, windowOpen, foreground, !busy else { return }
        resetRecipient()
        let token = recipientScanID
        perform { [self] operation in
            let request = try await engine.scanRecipientImage(url)
            try requireCurrentWork(operation)
            guard foreground, protection == .recipient, recipientScanID == token else { return }
            acceptRecipient(request)
        }
    }

    /// This Mac's FrankenTerm machines and the keys their routes use, as the
    /// desktop is configured now. Runs when the window first opens.
    func loadThisMac() {
        guard !busy, sourceSheet == nil else { return }
        cancelRecipientScan(); retireSourceKeyPrompt(); retireSSHSelection(); invalidatePrepared()
        perform { [self] operation in
            let result = try await engine.loadThisMac()
            try requireCurrentWork(operation)
            apply(result.state)
            let machines = result.state.selection.workspace.machines.count
            var text = "Loaded \(machines) machines from \(result.inventory) with the \(result.state.loadedFingerprints.count) keys this Mac uses for them."
            if !result.unresolved.isEmpty {
                text += " Still to decide: " + result.unresolved.map(\.reason).joined(separator: "; ") + "."
            }
            message = text + " Approve the keys below, then Prepare Encrypted Transfer."
        }
    }
    private func loadDesktopHandoff() {
        guard !busy, sourceSheet == nil else { return }
        clearVisibleMaterial()
        perform { [self] operation in
            let result = try await engine.loadDesktopHandoff()
            try requireCurrentWork(operation)
            guard let runtime = result.runtime else { throw FTError.malformed("machines.runtime_witness") }
            apply(result.state)
            message = "Captured \(result.state.selection.workspace.machines.count) machines from the running desktop's loaded inventory at configuration \(runtime.configurationGeneration). Review the source scope and selected keys before preparing the transfer."
            if !result.unresolved.isEmpty {
                message = (message ?? "") + " Still to decide: " + result.unresolved.map(\.reason).joined(separator: "; ") + "."
            }
        }
    }
    func chooseWorkspace() {
        guard !busy, sourceSheet == nil else { return }
        cancelRecipientScan()
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.json, .data]; panel.message = "Choose a local .ftworkspace JSON document. Executable Mac configuration is never evaluated."
        guard panel.runModal() == .OK, let url = panel.url, windowOpen, foreground, !busy else { return }
        retireSourceKeyPrompt(); retireSSHSelection(); invalidatePrepared()
        perform { [self] operation in
            let result = try await engine.loadWorkspace(url)
            try requireCurrentWork(operation)
            apply(result)
        }
    }
    func chooseEncryptedTransfer() {
        guard !busy, sourceSheet == nil else { return }
        cancelRecipientScan()
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.data]
        panel.message = "Choose a local .ftairgap file to replay or repair its encrypted QR transfer. Its original code or matching iPhone request key is still required."
        guard panel.runModal() == .OK, let url = panel.url, windowOpen, foreground, !busy else { return }
        retireSourceKeyPrompt(); retireSSHSelection(); invalidatePrepared()
        perform { [self] operation in
            let result = try await engine.openEncryptedTransfer(url)
            try requireCurrentWork(operation)
            guard foreground else { await engine.invalidateExport(); throw CancellationError() }
            clearVisibleMaterial()
            prepared = result
            message = "Encrypted transfer opened. Playback and repair reuse these exact saved bytes. Verify its comparison value on the iPhone before restoring."
        }
    }
    func chooseSSHConfiguration() {
        guard !busy, sourceSheet == nil else { return }
        cancelRecipientScan()
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.showsHiddenFiles = true
        panel.directoryURL = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".ssh")
        panel.message = "Choose a user SSH config, then select its hosts. Local Include files, public identities, explicit CertificateFile certificates, and each host's known-hosts files are read within fixed limits. When a .pub file is missing, a supported key container can supply its public identity. Including or unlocking private keys requires a separate choice. Relative Include paths use ~/.ssh; commands and Match conditions are never executed."
        guard panel.runModal() == .OK, let url = panel.url, windowOpen, foreground, !busy else { return }
        retireSourceKeyPrompt(); retireSSHSelection(); invalidatePrepared()
        perform { [self] operation in
            let aliases = try await engine.inspectSSHConfiguration(url)
            do { try requireCurrentWork(operation) }
            catch { await engine.cancelSSHSelection(); throw error }
            guard foreground else { await engine.cancelSSHSelection(); throw CancellationError() }
            sshAliases = aliases
            selectedSSHAliases = sshAliases.count <= 128 ? Set(sshAliases) : []
            let id = UUID(); sshSelectionID = id; sshSearch = ""; sourceSheet = .sshHosts(id)
        }
    }
    func selectSSHAlias(_ alias: String, included: Bool) {
        guard !busy else { return }
        if included, selectedSSHAliases.count < 128 { selectedSSHAliases.insert(alias) }
        else if !included { selectedSSHAliases.remove(alias) }
    }
    func importSelectedSSHHosts() {
        guard !busy, sshSelectionID != nil, !selectedSSHAliases.isEmpty, selectedSSHAliases.count <= 128 else { return }
        let aliases = sshAliases.filter { selectedSSHAliases.contains($0) }
        cancelRecipientScan(); retireSourceKeyPrompt(); invalidatePrepared()
        perform { [self] operation in
            let result = try await engine.loadSSHSelection(aliases)
            try requireCurrentWork(operation)
            apply(result)
            retireSSHSelection()
        }
    }
    func dismissSSHSelection(id: UUID? = nil) {
        guard !busy, let current = sshSelectionID, id == nil || id == current else { return }
        cancelRecipientScan(); retireSourceKeyPrompt(); retireSSHSelection()
        perform { [self] _ in await engine.cancelSSHSelection() }
    }
    func selectGroup(_ id: String, included: Bool) {
        guard !busy, sourceSheet == nil else { return }
        var groups = selectedGroupIDs
        if included { groups.insert(id) } else { groups.remove(id) }
        guard !groups.isEmpty else { return }
        cancelRecipientScan(); retireSourceKeyPrompt(); invalidatePrepared()
        perform { [self] operation in
            let result = try await engine.selectGroups(groups)
            try requireCurrentWork(operation)
            apply(result)
        }
    }
    func selectMachine(_ id: String, included: Bool) {
        guard !busy, sourceSheet == nil else { return }
        var machines = selectedMachineIDs
        if included { machines.insert(id) } else { machines.remove(id) }
        guard !machines.isEmpty else { return }
        cancelRecipientScan(); retireSourceKeyPrompt(); invalidatePrepared()
        perform { [self] operation in
            let result = try await engine.selectMachines(machines)
            try requireCurrentWork(operation)
            apply(result)
        }
    }
    func decide(_ value: WorkspaceExportSelection.IdentityDecision, fingerprint: String) {
        guard !busy, sourceSheet == nil else { return }
        cancelRecipientScan(); retireSourceKeyPrompt(); invalidatePrepared()
        perform { [self] operation in
            let result = try await engine.decide(value, fingerprint: fingerprint)
            try requireCurrentWork(operation)
            apply(result)
        }
    }
    func configuredKey(for fingerprint: String) -> WorkspaceExportSelection.ConfiguredIdentityFile? {
        selection?.configuredIdentityFile(for: fingerprint, fileReferences: identityReferences)
    }
    func useConfiguredKey(_ file: WorkspaceExportSelection.ConfiguredIdentityFile) {
        guard !busy, foreground, windowOpen, NSApplication.shared.isActive,
              sourceSheet == nil, sourceKeyOperation == nil,
              identityDecisions[file.fingerprint] == .includePrivateKey,
              configuredKey(for: file.fingerprint) == file else { return }
        startKeyLoad(.configuredFile(file))
    }
    private enum KeySource {
        case selectedFile(URL, credentialID: String)
        case configuredFile(WorkspaceExportSelection.ConfiguredIdentityFile)
    }
    func chooseKey(_ credentialID: String) {
        guard !busy, sourceSheet == nil else { return }
        cancelRecipientScan()
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.showsHiddenFiles = true
        if let reference = identityReferences[credentialID], let hint = MacExportEngine.identityURL(reference) {
            let privateHint = hint.pathExtension == "pub" ? hint.deletingPathExtension() : hint
            panel.directoryURL = privateHint.deletingLastPathComponent()
            panel.nameFieldStringValue = privateHint.lastPathComponent
        }
        panel.message = "Choose the matching OpenSSH Ed25519, ECDSA or RSA key, or an ECDSA/RSA PEM key. ECDSA supports P-256/P-384/P-521; RSA supports 2048–8192 bits with SHA-2 signatures. Protected OpenSSH keys and PKCS#8 files using PBES2, PBKDF2 and AES-CBC are unlocked locally after you enter their passphrase. Clear SEC1, PKCS#1 and PKCS#8 files are also supported."
        guard panel.runModal() == .OK, let url = panel.url, windowOpen, foreground, !busy else { return }
        startKeyLoad(.selectedFile(url, credentialID: credentialID))
    }
    private func startKeyLoad(_ source: KeySource) {
        cancelRecipientScan(); retireSourceKeyPrompt(); retireSSHSelection(); invalidatePrepared()
        let token = UUID(); sourceKeyOperation = token
        perform { [self] operation in
            do {
                let result: MacExportEngine.KeyLoadResult
                switch source {
                case let .selectedFile(url, credentialID):
                    result = try await engine.loadKey(url, credentialID: credentialID, operationID: token)
                case let .configuredFile(file):
                    result = try await engine.loadConfiguredKey(file, operationID: token)
                }
                try requireCurrentWork(operation)
                guard foreground, sourceKeyOperation == token else { throw CancellationError() }
                switch result {
                case let .loaded(state):
                    retireSourceKeyPrompt(); apply(state)
                    await engine.acknowledgeKey(operationID: token)
                case let .passphraseRequired(request):
                    keyUnlockRequest = request; sourceSheet = .keyUnlock(request)
                }
            } catch {
                await engine.cancelKeyUnlock(operationID: token)
                if sourceKeyOperation == token { retireSourceKeyPrompt() }
                throw error
            }
        }
    }
    func unlockSourceKey(requestID: UUID, passphrase: Data) {
        guard !busy, foreground, sourceSheet?.id == requestID, keyUnlockRequest?.id == requestID,
              let token = sourceKeyOperation else { return }
        perform { [self] operation in
            var secret = passphrase
            defer { secret.resetBytes(in: 0..<secret.count) }
            do {
                let result = try await engine.unlockKey(operationID: token, requestID: requestID, passphrase: secret)
                try requireCurrentWork(operation)
                guard foreground, sourceSheet?.id == requestID, sourceKeyOperation == token,
                      keyUnlockRequest?.id == requestID else { throw CancellationError() }
                retireSourceKeyPrompt(); apply(result)
                await engine.acknowledgeKey(operationID: token)
            } catch {
                let remaining = await engine.pendingKeyRequest(operationID: token)
                if !Task.isCancelled, workOperationID == operation, foreground, sourceSheet?.id == requestID,
                   sourceKeyOperation == token, keyUnlockRequest?.id == requestID,
                   let remaining {
                    keyUnlockRequest = remaining; sourceSheet = .keyUnlock(remaining)
                } else {
                    await engine.cancelKeyUnlock(operationID: token, requestID: requestID)
                    if sourceKeyOperation == token { retireSourceKeyPrompt() }
                }
                throw error
            }
        }
    }
    func cancelKeyUnlock(requestID: UUID) {
        guard keyUnlockRequest?.id == requestID, let token = sourceKeyOperation else { return }
        work?.cancel(); retireSourceKeyPrompt()
        // Bind cancellation to the dismissed request rather than the next file.
        Task { await engine.cancelKeyUnlock(operationID: token, requestID: requestID) }
    }
    /// The one button: these keys are approved by pressing it, and the
    /// animation starts as soon as the transfer is sealed.
    func sendToPhone() {
        guard !busy, exportReview?.canPrepare == true, destinationReady else { return }
        approveSelectedKeys = true
        playWhenPrepared = true
        prepare()
    }
    func prepare() {
        guard !busy, foreground, sourceSheet == nil, sourceKeyOperation == nil,
              approveSelectedKeys, exportReview?.canPrepare == true, destinationReady else { return }
        let mode = protection
        let recipient = mode == .recipient ? recipientRequest : nil
        let digest = mode == .recipient ? confirmedRecipientDigest : nil
        retireSourceKeyPrompt()
        pause(); prepared = nil; emitted = 0; repairPage = -1
        let token = generation
        perform { [self] operation in
            do {
                let result = try await engine.prepare(protection: mode, recipient: recipient, confirmedRequestDigest: digest)
                guard !Task.isCancelled, workOperationID == operation, foreground, generation == token else {
                    await engine.invalidateExport(); throw CancellationError()
                }
                prepared = result; emitted = 0; repairPage = -1
                if playWhenPrepared {
                    playWhenPrepared = false
                    // After this work settles (busy clears), not inside it.
                    let exportID = result.id, presentation = generation
                    Task { @MainActor [weak self] in
                        guard let self, self.prepared?.id == exportID, self.generation == presentation else { return }
                        self.play()
                    }
                }
            } catch let error as FTError where mode == .recipient && error == .authentication {
                guard !Task.isCancelled, workOperationID == operation, foreground, generation == token else {
                    await engine.invalidateExport(); throw CancellationError()
                }
                recipientRequest = nil; recipientComparison = nil; confirmedRecipientDigest = nil
                await engine.invalidateExport()
                try requireCurrentWork(operation)
                guard foreground, generation == token else { throw CancellationError() }
                message = "The recipient request could not be used for encryption. Create a new request on the iPhone, then scan and compare it again."
            }
        }
    }
    private func requireCurrentWork(_ operation: UUID) throws {
        guard !Task.isCancelled, windowOpen, workOperationID == operation else { throw CancellationError() }
    }
    /// `resumingPlayback`: a save paused the animation; it plays again once
    /// the save settles, however it ends.
    private func perform(resumingPlayback: Bool = false, _ body: @escaping @MainActor (UUID) async throws -> Void) {
        guard !busy, windowOpen else { return }
        let operation = UUID()
        workOperationID = operation; busy = true; message = nil
        work = Task {
            defer {
                if workOperationID == operation { busy = false; work = nil; workOperationID = nil }
                if resumingPlayback { play() }
            }
            do { try requireCurrentWork(operation); try await body(operation) }
            catch {
                if windowOpen, workOperationID == operation { report(error) }
            }
        }
    }
    func selectRate(_ value: Int) {
        guard !busy, [3, 5, 10].contains(value), value != rate else { return }
        rate = value; playbackSettings = UUID()
    }
    func selectRepairPage(_ value: Int) {
        guard !busy, let prepared, value == -1 || (0..<prepared.pages).contains(value), value != repairPage else { return }
        repairPage = value; playbackSettings = UUID()
    }
    func revealTransferCode() {
        guard !busy, foreground, let prepared, prepared.hasTransferCode else { return }
        pause()
        let token = codeRevealEpoch, exportID = prepared.id
        perform { [self] operation in
            let revealed = try await engine.revealTransferCode(exportID: exportID)
            try requireCurrentWork(operation)
            let image = try await renderer.codeSymbol(text: revealed.frame)
            try requireCurrentWork(operation)
            guard foreground, sessionActive, screensAwake, codeRevealEpoch == token,
                  self.prepared?.id == exportID else { throw CancellationError() }
            revealedTransferCode = revealed.code; revealedCodeSymbol = image
        }
    }
    func hideTransferCode() {
        codeRevealEpoch = UUID(); revealedTransferCode = nil; revealedCodeSymbol = nil
    }
    /// Regular playback contains only encrypted FTQR1 data. A generated code
    /// is revealed separately while this surface is paused and covered.
    func play() {
        guard !busy, !playing, let prepared, windowOpen, sessionActive, screensAwake else { return }
        let prior = playback
        pause(); playing = true
        let token = generation, exportID = prepared.id, engine = self.engine
        awake = ProcessInfo.processInfo.beginActivity(options: [.idleDisplaySleepDisabled], reason: "Display encrypted workspace QR transfer")
        playback = Task { [weak self] in
            await prior?.value
            guard !Task.isCancelled, let self, self.playing, self.generation == token,
                  self.windowOpen, self.prepared?.id == exportID else { return }
            do {
                let lease = try await engine.beginPlayback(exportID: exportID, rate: self.rate,
                    repairPage: self.repairPage < 0 ? nil : self.repairPage)
                do {
                    while !Task.isCancelled {
                        guard self.playing, self.generation == token, self.windowOpen,
                              self.prepared?.id == exportID else { break }
                        let settings = self.playbackSettings
                        let frame = try await engine.next(exportID: exportID, playback: lease, rate: self.rate,
                            repairPage: self.repairPage < 0 ? nil : self.repairPage)
                        let image = try await self.renderer.symbol(text: frame.text)
                        guard !Task.isCancelled, self.playing, self.generation == token, self.windowOpen,
                              self.prepared?.id == exportID else { break }
                        guard settings == self.playbackSettings else { continue }
                        self.symbol = image; self.emitted += 1
                        try await Task.sleep(for: .milliseconds(1000 / frame.symbolsPerSecond))
                    }
                } catch {
                    if !Task.isCancelled, self.generation == token, self.windowOpen {
                        self.pause(); self.report(error)
                    }
                }
                await engine.pausePlayback(exportID: exportID, playback: lease)
            } catch {
                if !Task.isCancelled, self.generation == token, self.windowOpen {
                    self.pause(); self.report(error)
                }
            }
        }
    }
    func pause() {
        generation = UUID(); playback?.cancel(); playing = false; symbol = nil; hideTransferCode()
        if let awake { ProcessInfo.processInfo.endActivity(awake); self.awake = nil }
    }
    /// Done sending: the window goes back to the machines.
    func stopSending() {
        guard !busy else { return }
        invalidatePrepared()
        perform { [self] _ in await engine.invalidateExport() }
    }
    func openExport(isActive: Bool) {
        windowOpen = true
        setForeground(isActive)
        if !loadedAtLaunch, CommandLine.arguments.contains("--desktop-source-stdin") {
            // An invalid/retired handoff cannot silently turn into a fresh read
            // of files on disk. The user may deliberately choose Other Sources.
            loadedAtLaunch = true
            loadDesktopHandoff()
        } else if !loadedAtLaunch, document == nil, MacExportEngine.machineSourceCommand() != nil {
            loadedAtLaunch = true
            loadThisMac()
        }
    }
    func setSessionActive(_ value: Bool) {
        sessionActive = value
        if !value { pause() }
    }
    func setScreensAwake(_ value: Bool) {
        screensAwake = value
        if !value { pause() }
    }
    func setForeground(_ value: Bool) {
        foreground = value && windowOpen
        if !foreground {
            // A generated code belongs to the focused window. Ciphertext may
            // continue playing while the user looks at the phone; display sleep,
            // session lock and window closure pause it explicitly.
            hideTransferCode()
            cancelRecipientScan()
            if let token = sourceKeyOperation {
                work?.cancel(); retireSourceKeyPrompt()
                // The bounded worker checks cancellation before installing its
                // result; retire the selected source once that worker returns.
                Task { await engine.cancelKeyUnlock(operationID: token) }
            }
        }
    }
    func saveEncryptedFile() {
        guard !busy, let prepared else { return }
        let resume = playing, exportID = prepared.id
        pause()
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-Workspace.ftairgap"
        let recipientOnly = prepared.protection == .recipient
        panel.message = recipientOnly
            ? "Save encrypted bytes for the confirmed iPhone. This file will not open after the phone loses its active request key. Choose a new filename."
            : "Save encrypted bytes only. Keep the separately revealed transfer code in a different place. Choose a new filename."
        guard panel.runModal() == .OK, let url = panel.url, windowOpen, foreground, !busy else {
            if resume { play() }; return
        }
        perform(resumingPlayback: resume) { [self] operation in
            try await engine.saveEncryptedFile(exportID: exportID, to: url)
            try requireCurrentWork(operation)
            message = recipientOnly
                ? "Encrypted workspace saved for the confirmed iPhone. Its active request key is required; this file is not an independent recovery backup."
                : "Encrypted workspace saved. The transfer code is not included in this file."
        }
    }
    func savePublicWorkspace() {
        guard !busy, sourceSheet == nil, selection != nil else { return }
        cancelRecipientScan()
        pause()
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-Workspace.ftworkspace"
        panel.message = "Save selected hosts, groups, public identities, and trust policy without private keys. The document is unencrypted. Check its file comparison value on your phone before importing. Choose a new filename."
        guard panel.runModal() == .OK, let url = panel.url, windowOpen, foreground, !busy else { return }
        perform { [self] operation in
            let comparison = try await engine.savePublicWorkspace(to: url)
            try requireCurrentWork(operation)
            message = "Public workspace saved without private keys. Compare this exact value on your phone through a trusted separate channel: " + comparison
        }
    }
    func saveAnimation() {
        guard !busy, let prepared else { return }
        let resume = playing, exportID = prepared.id
        pause()
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-QR.apng"
        panel.allowedContentTypes = [.png]; panel.allowsOtherFileTypes = true
        let recipientOnly = prepared.protection == .recipient
        panel.message = "Save a lossless animated PNG. Every encoded frame is checked before the final file is published. Choose a new filename."
            + (recipientOnly ? " The confirmed iPhone must retain its active request key; this is not an independent recovery backup." : " Keep its transfer code separate.")
        guard panel.runModal() == .OK, let url = panel.url, windowOpen, foreground, !busy else {
            if resume { play() }; return
        }
        let speed = rate
        perform(resumingPlayback: resume) { [self] operation in
            let bytes = try await engine.encryptedEnvelope(exportID: exportID)
            try requireCurrentWork(operation)
            let count = try await renderer.saveAnimation(envelope: bytes, to: url, symbolsPerSecond: speed)
            try requireCurrentWork(operation)
            message = "Saved and verified \(count) encrypted QR symbols. Play the APNG without interpolation. "
                + (recipientOnly ? "The confirmed iPhone must retain its active request key; this is not an independent recovery backup." : "Keep its transfer code separate.")
        }
    }
    func saveVideo() {
        guard !busy, let prepared else { return }
        let resume = playing, exportID = prepared.id
        pause()
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-QR.mp4"
        panel.allowedContentTypes = [.mpeg4Movie]
        let recipientOnly = prepared.protection == .recipient
        panel.message = "Save an encrypted QR video. Every decoded frame and the reconstructed package must pass verification before the new file is published."
            + (recipientOnly ? " The confirmed iPhone must retain its active request key; this is not an independent recovery backup." : " Keep its transfer code separate.")
        guard panel.runModal() == .OK, let url = panel.url, windowOpen, foreground, !busy else {
            if resume { play() }; return
        }
        let speed = rate
        perform(resumingPlayback: resume) { [self] operation in
            let bytes = try await engine.encryptedEnvelope(exportID: exportID)
            try requireCurrentWork(operation)
            let source = try OpticalVideoSource(envelope: bytes)
            let receipt = try await videoRenderer.save(source: source, to: url, symbolsPerSecond: speed)
            try requireCurrentWork(operation)
            let duration = String(format: "%.1f", receipt.durationSeconds)
            message = "Saved and verified \(receipt.symbolCount) encrypted QR symbols in a \(duration)-second MP4 (\(receipt.frameCount) frames). "
                + (recipientOnly ? "The confirmed iPhone must retain its active request key; this is not an independent recovery backup." : "Keep its transfer code separate.")
        }
    }
    func cancelWork() {
        guard !closingCleanup else { return }
        work?.cancel()
        if let token = sourceKeyOperation {
            retireSourceKeyPrompt()
            Task { await engine.cancelKeyUnlock(operationID: token) }
        }
    }
    func clear() {
        guard !busy else { return }
        clearVisibleMaterial()
        perform { [self] _ in await engine.clear() }
    }
    private func clearVisibleMaterial() {
        retireSourceKeyPrompt(); retireSSHSelection(); resetRecipient(); protection = .code
        document = nil; selection = nil; identityDecisions = [:]; exportReview = nil
        includedFingerprints = []; prepared = nil; approveSelectedKeys = false; repairPage = -1
        identityReferences = [:]; message = nil
    }
    func closeExport() {
        guard windowOpen else { return }
        windowOpen = false; foreground = false
        let priorWork = work
        priorWork?.cancel()
        clearVisibleMaterial()
        // A bounded KDF or native encoder may need time to observe cancellation.
        // Its old completion must not publish state or release cleanup's lock.
        let cleanup = UUID()
        workOperationID = cleanup; closingCleanup = true; busy = true
        work = Task {
            // Media workers hold ciphertext already; release private keys now.
            await engine.clear()
            await priorWork?.value
            // Clear any old queued engine mutation before allowing a reopened
            // window to load another workspace, source selection, or identity.
            await engine.clear()
            guard workOperationID == cleanup else { return }
            closingCleanup = false; busy = false; work = nil; workOperationID = nil
        }
    }
    func closeAndWait() async {
        closeExport()
        let cleanup = work
        await cleanup?.value
    }
    private func report(_ error: Error) {
        if let error = error as? LocalHelperProcessError {
            message = "The running desktop snapshot could not be received: \(error.localizedDescription) Choose Export to iPhone again in the desktop, or deliberately select another source."
        } else if let error = error as? MachineSourceError {
            message = error.localizedDescription
        } else if let error = error as? LocalExportFileError {
            message = error.localizedDescription
        } else if let error = error as? OpenSSHSourceKeyError {
            message = error.localizedDescription
        } else if let error = error as? FTError {
            if error == .malformed("export.recipient_confirmation") || error == .unsupported("recipient.confirmation_required") {
                message = "Scan the phone’s recipient request and confirm its comparison value before preparing this transfer."
            } else if error == .limit("source_key.kdf_parameters") || error == .limit("key.source_kdf") {
                message = "This key's protection settings exceed the supported unlock limits (bcrypt rounds 1–64 and salt 16–64 bytes). Choose another matching supported source file or keep its public reference."
            } else if error == .unsupported("source_key.cipher") || error == .unsupported("source_key.kdf") {
                message = "This protected key uses an unsupported format. Source unlocking supports OpenSSH bcrypt with AES-CTR or AES-CBC using 128-, 192-, or 256-bit keys."
            } else if error == .unsupported("source_key.encrypted_pem") || error == .unsupported("source_key.legacy_pem_cipher") {
                message = "This PEM protection is not supported. Traditional RSA and EC files must use AES-CBC with 128-, 192- or 256-bit keys. Protected OpenSSH and supported PBES2 PKCS#8 files can also be unlocked."
            } else if error == .limit("source_key.legacy_pem_ciphertext") {
                message = "This encrypted PEM file exceeds the supported single-key size. Choose another matching supported key file."
            } else if [.unsupported("source_key.pkcs8_encryption"), .unsupported("source_key.pkcs8_kdf"),
                       .unsupported("source_key.pkcs8_prf"), .unsupported("source_key.pkcs8_cipher")].contains(error) {
                message = "This PKCS#8 file uses unsupported protection. Supported files use PBES2, PBKDF2 with HMAC-SHA1, SHA256, SHA384 or SHA512, and AES-CBC with 128-, 192- or 256-bit keys."
            } else if error == .limit("source_key.pbkdf2_parameters") {
                message = "This PKCS#8 file exceeds the supported unlock limits: 1–1,000,000 PBKDF2 iterations and an 8–64 byte salt. Choose another matching supported key file or keep its public reference."
            } else if error == .limit("source_key.pkcs8_der") {
                message = "This encrypted PKCS#8 file exceeds the supported single-key size. Choose another matching supported key file or keep its public reference."
            } else if error == .limit("source_key.passphrase") {
                message = "Enter a passphrase between 1 and 1,024 UTF-8 bytes."
            } else if error == .malformed("ssh.include_cycle") {
                message = "The selected SSH configuration includes itself through a file cycle. Resolve the cycle, then select the configuration again. No partial host list was imported."
            } else if error == .unsupported("ssh.include_permissions") {
                message = "An SSH configuration file has unsafe ownership or is writable by a group or other users. Use a source owned by you or the system with those write permissions removed."
            } else if error == .unsupported("ssh.include_not_local") {
                message = "An included SSH configuration is on a network volume or is not downloaded locally. Choose an already-local configuration before offline export."
            } else if error == .unsupported("ssh.include_file") || error == .unsupported("ssh.include_directory") {
                message = "An included SSH configuration could not be read. Check local access and permissions; symbolic links in Include paths are not followed. No partial host list was imported."
            } else if [.limit("ssh.include_bytes"), .limit("ssh.include_lines"), .limit("ssh.include_files_or_depth"),
                       .limit("ssh.include_matches"), .limit("ssh.include_directory_entries"), .limit("ssh.include_patterns"),
                       .limit("ssh.include_path_depth"), .limit("ssh.include_file")].contains(error) {
                message = "The SSH configuration exceeds the bounded Include profile: 64 file reads, eight nested includes, 2 MiB of source, 16,384 lines, and 16,384 scanned directory entries. Choose a smaller explicit configuration; no partial source was imported."
            } else if error == .limit("animation.use_live_transfer") {
                message = "This package exceeds the 256-symbol saved-animation limit. Use live QR playback or save the encrypted .ftairgap file. No settings or keys were omitted."
            } else if error == .limit("envelope.bytes") || error == .limit("archive.bytes") {
                message = "The selected package exceeds this transfer profile: at most 4 MiB encrypted and 8 MiB expanded. Deselect machines in the preview or keep selected identities as references only, then prepare a new transfer. No machine, setting, or key was omitted automatically."
            } else if error == .malformed("export.machine_selection") {
                message = "Keep at least one machine selected from the chosen groups. Select a group first to include one of its machines."
            } else if error == .limit("video.use_live_transfer") {
                message = "This package exceeds the bounded video duration or symbol count. Select fewer machines, groups or identities, use live playback, or save the encrypted .ftairgap file. The package was not truncated."
            } else if error == .unsupported("video.published_durability_unconfirmed") {
                message = "The MP4 was verified and saved. The filesystem could not confirm that the directory change would survive a sudden system failure. The completed movie is retained at the selected destination."
            } else if error == .unsupported("ssh.no_concrete_hosts") {
                message = "This config and its supported local Include files have no concrete Host aliases to select. Add named hosts to the selected configuration; wildcard rules alone do not identify a finite machine list."
            } else if [.limit("ssh.known_hosts.files"), .limit("ssh.known_hosts.bytes"), .limit("ssh.known_hosts.lines")].contains(error) {
                message = "The selected known-hosts sources exceed 64 files, 2 MiB, or 16,384 lines. Select fewer hosts or a smaller explicit trust-file policy. No partial trust or revocation list was imported."
            } else if error == .unsupported("recipient.camera") {
                message = "A camera with QR scanning is unavailable. Choose a local image of the phone’s recipient-request QR, or use a generated transfer code."
            } else if error == .unsupported("recipient.camera_interrupted") {
                message = "The camera stopped. Start the recipient scanner again when the camera is available, or choose a local QR image."
            } else if error == .malformed("recipient.multiple_qr") {
                message = "More than one recipient request is visible. Show only the QR request on the intended iPhone and scan again."
            } else if error == .limit("recipient.image") || error == .malformed("recipient.image") {
                message = "Choose a single local PNG, JPEG, or HEIC image no larger than 8 MiB and 16 megapixels. A screenshot of the recipient request works well."
            } else if error == .malformed("recipient.image_qr") {
                message = "The image must contain exactly one readable FrankenTerm recipient-request QR. Choose another image or scan the phone with the camera."
            } else if error == .limit("recipient.envelope_limit") {
                message = "This export exceeds the size accepted by the iPhone’s request. Select fewer machines, groups or private identities, or create and confirm a new request with a larger limit. Nothing was omitted automatically."
            } else if error == .authentication, protection == .recipient {
                message = "The recipient request could not be used for encryption. Create a new request on the iPhone, then scan and compare it again."
            } else { message = error.localizedDescription }
        }
        else if !(error is CancellationError) { message = "The operation could not finish. Check access, available storage, and choose a new filename. Existing source files were not changed." }
    }
}

@MainActor
private struct MacExportView: View {
    @Bindable var model: MacExportModel
    @Bindable var enrollment: MacPhoneKeyEnrollmentModel
    @Environment(\.scenePhase) private var scenePhase
    var body: some View {
        Group {
            if model.prepared != nil {
                sending
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 20) {
                        HStack {
                            Label("FrankenTerm", systemImage: "terminal.fill").font(.largeTitle.bold())
                            Spacer()
                            Menu("Other Sources") {
                                Button("Reload This Mac's Machines") { model.loadThisMac() }
                                Button("Import SSH Config…") { model.chooseSSHConfiguration() }
                                Button("Open Workspace…") { model.chooseWorkspace() }
                                Button("Open Encrypted Transfer…") { model.chooseEncryptedTransfer() }
                            }.fixedSize().disabled(model.busy || enrollment.busy)
                        }
                        if let message = model.message { Text(verbatim: message).font(.callout).textSelection(.enabled) }
                        if let document = model.document {
                            exportChoices(document).disabled(enrollment.busy)
                        } else if model.busy {
                            ProgressView("Loading this Mac's machines…").frame(maxWidth: .infinity)
                        } else {
                            ContentUnavailableView("No machines loaded", systemImage: "rectangle.3.group",
                                description: Text("This Mac's FrankenTerm machines load automatically. Use Other Sources to open a workspace or SSH config instead."))
                        }
                        enrollmentCard.disabled(model.busy || model.sourceSheet != nil || model.requestingCameraAccess)
                    }.padding(28)
                }
            }
        }.frame(minWidth: 760, minHeight: 680)
            .sheet(item: $model.sourceSheet) { sheet in
                Group {
                    switch sheet {
                    case .sshHosts(_): sshSelection
                    case let .keyUnlock(request): SourceKeyUnlockSheet(model: model, request: request).id(request.id)
                    case let .recipientCamera(scanID): recipientCamera(scanID: scanID)
                    }
                }.onDisappear { model.sourceSheetDismissed(sheet) }
            }
            .onAppear {
                enrollment.open(isActive: scenePhase == .active)
                model.openExport(isActive: scenePhase == .active)
            }
            .onChange(of: model.selection?.workspace.raw, initial: true) { _, _ in
                enrollment.sourceChanged(model.selection?.workspace, machines: model.selection?.orderedMachines ?? [])
            }
            .onChange(of: model.selection?.orderedMachines.map(\.id) ?? [], initial: true) { _, _ in
                enrollment.sourceChanged(model.selection?.workspace, machines: model.selection?.orderedMachines ?? [])
            }
            .onChange(of: scenePhase, initial: true) { _, phase in
                enrollment.setActive(phase == .active)
                model.setForeground(phase == .active)
                if phase == .background { model.pause() }
            }
            .onReceive(NSWorkspace.shared.notificationCenter.publisher(for: NSWorkspace.sessionDidResignActiveNotification)
                .receive(on: RunLoop.main)) { _ in enrollment.setSessionActive(false); model.setSessionActive(false) }
            .onReceive(NSWorkspace.shared.notificationCenter.publisher(for: NSWorkspace.sessionDidBecomeActiveNotification)
                .receive(on: RunLoop.main)) { _ in enrollment.setSessionActive(true); model.setSessionActive(true) }
            .onReceive(NSWorkspace.shared.notificationCenter.publisher(for: NSWorkspace.screensDidSleepNotification)
                .receive(on: RunLoop.main)) { _ in enrollment.setScreensAwake(false); model.setScreensAwake(false) }
            .onReceive(NSWorkspace.shared.notificationCenter.publisher(for: NSWorkspace.screensDidWakeNotification)
                .receive(on: RunLoop.main)) { _ in enrollment.setScreensAwake(true); model.setScreensAwake(true) }
            .onDisappear { enrollment.close(); model.closeExport() }
    }

    private var enrollmentCard: some View {
        VStack(alignment: .leading, spacing: 12) {
            Divider()
            Label("Phone Key Access", systemImage: "key.horizontal").font(.title2.bold())
            Text("Authorize or remove an enrollment for an independent phone identity on the accounts you select. Each change uses this Mac's existing trusted SSH access.")
                .font(.callout).foregroundStyle(.secondary)
            HStack {
                Button("Open Phone Key Request…") { enrollment.chooseRequest() }.disabled(enrollment.busy)
                if enrollment.busy {
                    ProgressView().controlSize(.small)
                    Button("Stop Enrollment Work") { enrollment.cancel() }
                }
            }
            if let request = enrollment.request {
                Text(verbatim: request.key.fingerprint).font(.system(.callout, design: .monospaced)).textSelection(.enabled)
                DisclosureGroup("Public Phone Key") {
                    Text(verbatim: request.key.authorizedKey).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    Text(verbatim: "Enrollment request " + request.id)
                        .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                }
                if let selection = model.selection {
                    Button("Review Selected Accounts") {
                        enrollment.review(document: selection.workspace, machines: selection.orderedMachines)
                    }.disabled(enrollment.busy)
                } else {
                    Text("Load a machine inventory or workspace to review its preferred accounts.").font(.callout)
                }
                if !enrollment.rows.isEmpty {
                    HStack {
                        Button("Select All Available") { enrollment.selectAll() }
                        Button("Select None") { enrollment.selectNone() }
                        Text("\(enrollment.selectedTargetIDs.count) of \(enrollment.eligibleCount) available accounts selected")
                            .font(.caption).foregroundStyle(.secondary)
                    }.disabled(enrollment.busy)
                    LazyVStack(alignment: .leading, spacing: 14) {
                        ForEach(enrollment.rows) { row in enrollmentRow(row) }
                    }
                    Toggle("I verified this fingerprint against the phone or a trusted saved record", isOn: Binding(
                        get: { enrollment.confirmedFingerprint }, set: { enrollment.confirmFingerprint($0) }))
                        .disabled(enrollment.busy)
                    HStack {
                        Button("Authorize Phone Key…") { enrollment.authorize() }
                            .buttonStyle(.borderedProminent).disabled(!enrollment.canAct)
                        Button("Remove Enrolled Access…", role: .destructive) { enrollment.removeEnrolledAccess() }
                            .disabled(!enrollment.canAct)
                    }
                    HStack {
                        Button("Reconcile Selected Accounts") { enrollment.reconcile() }.disabled(!enrollment.canAct)
                        Button("Save Results…") { enrollment.saveResults() }.disabled(!enrollment.canSaveResults)
                    }
                    Text("Removal requires the original enrollment request. It disables only the unrestricted entry that enrollment installed, retaining it as a comment. Restricted entries and independently installed copies remain and are reported. Existing SSH sessions and remote processes continue.")
                        .font(.caption).foregroundStyle(.secondary)
                    Text("Removal also requires a Linux server whose filesystem supports exclusive access to the key file. Unsupported servers need their normal administrator workflow; their result remains separate from other accounts.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                Text("Direct accounts require Python 3.8 or later and usable existing SSH authentication on this Mac. Enrollment preserves existing authorized-key restrictions and permissions. Reconcile reads account state without adding or removing access. Verify server acceptance with a connection from the phone; custom authorization files and certificate issuers need their approved workflow.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let message = enrollment.message { Text(verbatim: message).font(.callout).textSelection(.enabled) }
        }
    }

    private func enrollmentRow(_ row: MacPhoneKeyEnrollmentRow) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(verbatim: row.machines.joined(separator: ", ")).font(.headline)
            if let target = row.target {
                Toggle(isOn: Binding(get: { enrollment.selectedTargetIDs.contains(target.id) },
                                     set: { enrollment.select(target.id, included: $0) })) {
                    Text(verbatim: "\(target.user) @ \(target.host) · port \(target.port)")
                }.disabled(enrollment.busy)
                if let result = row.result {
                    if let mutation = row.journal?.lastMutation(for: target.id) {
                        Text(verbatim: "Last requested change: " + Self.enrollmentAction(mutation)).font(.caption)
                    }
                    if let action = row.journal?.lastAction(for: target.id) {
                        Text(verbatim: "Latest operation: " + Self.enrollmentAction(action))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                    Text(verbatim: Self.enrollmentStatus(result.state)).font(.callout.bold())
                    Text(verbatim: result.detail).font(.caption).foregroundStyle(.secondary)
                } else { Text("Not attempted").font(.caption).foregroundStyle(.secondary) }
                if let request = enrollment.request {
                    DisclosureGroup("Receipt") {
                        Text(verbatim: "Operation " + request.operationID(for: target))
                            .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                    }
                }
            } else if let reason = row.unavailableReason {
                Label("Unavailable for direct enrollment", systemImage: "exclamationmark.triangle").font(.callout)
                Text(verbatim: reason).font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private static func enrollmentAction(_ action: WorkspaceKeyEnrollmentAction) -> String {
        switch action {
        case .inspect: "Read-only inspection"
        case .install: "Authorize public key"
        case .remove: "Remove this enrollment's access"
        }
    }

    private static func enrollmentStatus(_ state: WorkspaceKeyEnrollmentState) -> String {
        switch state {
        case .absent: "Key entry absent"
        case .installed: "Key entry installed and verified"
        case .alreadyPresent: "Existing key entry verified"
        case .removed: "Enrolled entry disabled; no live matching entry remains"
        case .stillAuthorized: "Other matching key entries remain"
        case .restricted: "Existing restrictions preserved"
        case .blocked: "Needs attention"
        case .uncertain: "Outcome unknown — reconcile this account"
        }
    }

    /// The sender knows only what it displays. Completion stays a deliberate
    /// user action; this offline screen never starts network discovery.
    private var sending: some View {
        Group {
            if let prepared = model.prepared {
                VStack(spacing: 14) {
                    HStack(alignment: .top) {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(prepared.isReplay ? "Replay encrypted transfer" : "Send to your iPhone").font(.title.bold())
                            Text("Package prepared · \(ByteCountFormatter.string(fromByteCount: Int64(prepared.bytes), countStyle: .file)) · \(prepared.pages) pages")
                                .font(.callout).foregroundStyle(.secondary)
                        }
                        Spacer()
                        Button("Done") { model.stopSending() }.disabled(model.busy)
                    }
                    if let code = model.revealedTransferCode, let image = model.revealedCodeSymbol {
                        transferCodeCard(code, symbol: image)
                    } else {
                        ZStack {
                            Rectangle().fill(.white)
                            if let symbol = model.symbol {
                                PixelPerfectQRCode(symbol: symbol).padding(20)
                            } else {
                                VStack(spacing: 12) {
                                    Image(systemName: "pause.circle").font(.system(size: 48))
                                    Text(model.busy ? "Preparing your transfer…" : "Playback paused").font(.title3)
                                    Text("Resume to display encrypted QR frames.").font(.callout)
                                }.foregroundStyle(.black)
                            }
                        }.aspectRatio(1, contentMode: .fit).frame(maxWidth: .infinity, maxHeight: .infinity)
                    }
                    HStack {
                        Button(model.playing ? "Pause" : "Resume Playback") {
                            if model.playing { model.pause() } else { model.play() }
                        }.buttonStyle(.borderedProminent)
                        if prepared.hasTransferCode {
                            Button(model.revealedTransferCode == nil ? "Show Transfer Code" : "Hide Transfer Code") {
                                if model.revealedTransferCode == nil { model.revealTransferCode() }
                                else { model.hideTransferCode() }
                            }
                        }
                        Spacer()
                        Text("\(model.emitted) symbols displayed").font(.caption).monospacedDigit().foregroundStyle(.secondary)
                    }.disabled(model.busy)
                    HStack(spacing: 24) {
                        Picker("Speed", selection: Binding(get: { model.rate }, set: { model.selectRate($0) })) {
                            Text("3 / sec").tag(3); Text("5 / sec").tag(5); Text("10 / sec").tag(10)
                        }.pickerStyle(.segmented).frame(maxWidth: 300)
                        Picker("Repair page", selection: Binding(get: { model.repairPage }, set: { model.selectRepairPage($0) })) {
                            Text("All pages").tag(-1)
                            ForEach(0..<prepared.pages, id: \.self) { page in Text("Page \(page + 1)").tag(page) }
                        }.frame(maxWidth: 220)
                        Spacer(minLength: 0)
                    }.disabled(model.busy)
                    VStack(spacing: 4) {
                        Text("In FrankenTerm on the iPhone, choose Scan My Mac. Keep scanning until the phone has the complete transfer.")
                        if prepared.protection == .code {
                            Text(prepared.hasTransferCode
                                ? "Then choose Show Transfer Code and scan or enter it on the phone. The code is never part of playback or a saved recording."
                                : "Enter the original transfer code on the phone. An encrypted file cannot recover a lost code.")
                        } else {
                            Text("The iPhone must still hold the matching Only This Phone request key.")
                        }
                        Text(verbatim: "Compare on your iPhone: \(prepared.comparison)")
                            .font(.system(.callout, design: .monospaced)).textSelection(.enabled)
                        Text("Waiting for you to finish on the phone").foregroundStyle(.secondary)
                    }.font(.caption).multilineTextAlignment(.center)
                    backupControls(prepared)
                    if let message = model.message { Text(verbatim: message).font(.callout).textSelection(.enabled) }
                    if model.busy {
                        HStack { ProgressView().controlSize(.small); Button("Cancel Work") { model.cancelWork() } }
                    }
                }.padding(24)
            }
        }
    }

    private func transferCodeCard(_ code: String, symbol: QRCodeSymbol) -> some View {
        VStack(spacing: 16) {
            Label("Private transfer code", systemImage: "key.fill").font(.title2.bold())
            HStack(spacing: 24) {
                PixelPerfectQRCode(symbol: symbol).aspectRatio(1, contentMode: .fit)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .privacySensitive().accessibilityLabel("Private transfer code QR")
                VStack(alignment: .leading, spacing: 16) {
                    Text(verbatim: code).font(.system(.title2, design: .monospaced)).textSelection(.enabled)
                        .privacySensitive().accessibilityLabel("Private transfer code").accessibilityValue(code)
                    Text("Scan this separate QR or enter the code on your iPhone. Keep a separate copy if you save a backup; closing this export releases the code held by this app.")
                }.frame(maxWidth: 240)
            }
            Text("QR playback is paused and covered. A recording of both this code and the animation can unlock the transfer.")
                .font(.callout).foregroundStyle(.secondary).multilineTextAlignment(.center)
        }.padding(24).frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color(nsColor: .windowBackgroundColor), in: RoundedRectangle(cornerRadius: 16))
    }

    private func backupControls(_ prepared: MacExportEngine.Prepared) -> some View {
        DisclosureGroup("Save an encrypted backup") {
            VStack(alignment: .leading, spacing: 10) {
                Text(prepared.protection == .code
                    ? "Save the transfer code separately from these files. The same encrypted package can be restored from a file or a QR recording."
                    : "These files work only while the original iPhone request key remains available. For an independent recovery backup, prepare a new transfer using Generated Transfer Code.")
                    .font(.caption).foregroundStyle(.secondary)
                HStack {
                    Button("Encrypted File…") { model.saveEncryptedFile() }
                    Button("MP4 Video…") { model.saveVideo() }
                    Button("QR Animation…") { model.saveAnimation() }
                }.disabled(model.busy)
                Text("Saved recordings contain every planned source symbol and a finite repair set. For fresh repair symbols later, reopen the encrypted .ftairgap file on this Mac.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            .padding(.top, 8)
        }
    }

    private func recipientCamera(scanID: UUID) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Scan this iPhone’s request").font(.title2.bold())
            Text("On the iPhone, open the recipient-request QR in FrankenTerm. Hold it in front of this Mac’s camera. Only a FrankenTerm recipient request is accepted.")
                .font(.callout)
            RecipientRequestCamera(onRequest: { model.receiveRecipient($0, scanID: scanID) },
                                   onError: { model.recipientCameraFailed($0, scanID: scanID) })
                .id(scanID).frame(width: 640, height: 360).clipShape(RoundedRectangle(cornerRadius: 12))
            Text("Camera frames stay on this Mac. Scanning stops after one candidate; you must still compare and confirm its recipient value.")
                .font(.caption).foregroundStyle(.secondary)
            HStack { Spacer(); Button("Cancel Scan", role: .cancel) { model.cancelRecipientScan(scanID: scanID) } }
        }.padding(24).frame(width: 690)
    }

    private var sshSelection: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Choose SSH hosts").font(.title2.bold())
            Text("Select up to 128 named hosts. Matching public .pub files and each route's UserKnownHostsFile and GlobalKnownHostsFile settings supply public identities and host policy. Supported local Include files are already resolved. Unreadable trust files, commands, and Match conditions are reported before export; affected routes remain inactive.")
                .font(.callout).foregroundStyle(.secondary)
            TextField("Filter host aliases", text: $model.sshSearch).textFieldStyle(.roundedBorder)
            List(model.filteredSSHAliases, id: \.self) { alias in
                Toggle(isOn: Binding(get: { model.selectedSSHAliases.contains(alias) },
                    set: { model.selectSSHAlias(alias, included: $0) })) {
                    Text(verbatim: alias).font(.system(.body, design: .monospaced))
                }.disabled(model.busy || (model.selectedSSHAliases.count >= 128 && !model.selectedSSHAliases.contains(alias)))
            }.frame(minHeight: 260)
            HStack {
                Text("\(model.selectedSSHAliases.count) of \(model.sshAliases.count) hosts selected").font(.caption)
                Spacer()
                Button("Clear Selection") { model.selectedSSHAliases = [] }.disabled(model.busy)
                Button("Select All") { model.selectedSSHAliases = Set(model.sshAliases) }
                    .disabled(model.busy || model.sshAliases.count > 128)
            }
            if let message = model.message { Text(verbatim: message).font(.callout).textSelection(.enabled) }
            HStack {
                Button("Cancel") { model.dismissSSHSelection() }.disabled(model.busy)
                Spacer()
                if model.busy { ProgressView().controlSize(.small); Button("Cancel Work") { model.cancelWork() } }
                Button("Review Selected Hosts") { model.importSelectedSSHHosts() }
                    .buttonStyle(.borderedProminent).disabled(model.busy || model.selectedSSHAliases.isEmpty)
            }
        }.padding(24).frame(width: 620, height: 580).interactiveDismissDisabled(model.busy)
    }

    @ViewBuilder
    private func exportChoices(_ source: WorkspaceDocument) -> some View {
        if let selection = model.selection, let review = model.exportReview {
            sendSummary(selection, review)
        }
        DisclosureGroup("Details: groups, keys and host trust") {
            VStack(alignment: .leading, spacing: 20) { exportDetails(source) }.padding(.top, 10)
        }
    }

    /// The whole job at a glance: these machines, in their colors, and one button.
    @ViewBuilder
    private func sendSummary(_ selection: WorkspaceExportSelection, _ review: WorkspaceExportSelection.Review) -> some View {
        let sourceFindings = review.findings.filter {
            ($0.setting.hasPrefix("machines_toml.") || $0.setting.hasPrefix("desktop_snapshot."))
                && $0.setting != MachineCompanionSource.scopeSetting
                && $0.setting != MachineCompanionSource.desktopScopeSetting
        }
        let narrowed = !sourceFindings.isEmpty || !review.blockedRouteIDs.isEmpty
        VStack(alignment: .leading, spacing: 18) {
            VStack(alignment: .leading, spacing: 6) {
                Text("Send your machines to your iPhone").font(.title.bold())
                Text("\(selection.orderedMachines.count) machines and \(review.includedIdentityCount) keys from \(selection.workspace.name)")
                    .font(.title3).foregroundStyle(.secondary)
            }
            if let scope = review.findings.first(where: { $0.setting == MachineCompanionSource.desktopScopeSetting }) {
                Label("Captured from the running desktop", systemImage: "desktopcomputer").font(.headline)
                Text(verbatim: scope.reason).font(.caption).foregroundStyle(.secondary)
            }
            LazyVGrid(columns: [GridItem(.adaptive(minimum: 170), spacing: 10)], alignment: .leading, spacing: 10) {
                ForEach(selection.availableMachines) { machine in
                    let theme = model.document?.themes.first { $0.id == machine.themeID }
                    let included = model.selectedMachineIDs.contains(machine.id)
                    HStack(spacing: 8) {
                        Toggle(isOn: Binding(get: { model.selectedMachineIDs.contains(machine.id) },
                                             set: { model.selectMachine(machine.id, included: $0) })) {
                            Text(verbatim: machine.name)
                        }.labelsHidden().toggleStyle(.checkbox)
                            .accessibilityLabel("Include \(machine.name)")
                            .disabled(model.busy || (included && model.selectedMachineIDs.count == 1))
                        Circle().fill(Color(ftExportHex: theme?.accent ?? "#888888")).frame(width: 10, height: 10)
                        Text(verbatim: machine.name).lineLimit(1).foregroundStyle(Color(ftExportHex: theme?.foreground ?? "#FFFFFF"))
                        Spacer(minLength: 0)
                    }
                    .padding(.horizontal, 12).padding(.vertical, 10)
                    .background(Color(ftExportHex: theme?.background ?? "#1C1C1E"), in: RoundedRectangle(cornerRadius: 10))
                    .overlay(RoundedRectangle(cornerRadius: 10).stroke(Color(ftExportHex: theme?.accent ?? "#888888").opacity(0.45)))
                    .opacity(included ? 1 : 0.6)
                }
            }
            Text("Choose the machines to transfer. Shared jump hosts and their required keys stay included. \(selection.availableMachines.count - selection.orderedMachines.count) machines excluded.")
                .font(.caption).foregroundStyle(.secondary)
            Text("Selected public settings: \(ByteCountFormatter.string(fromByteCount: Int64(selection.workspace.raw.count), countStyle: .file)). The complete encrypted size and page count appear after preparation; the transfer limit is 4 MiB.")
                .font(.caption).foregroundStyle(.secondary)
            if !review.canPrepare {
                Label(review.pendingIdentityCount + review.missingApprovedKeyCount == 1
                      ? "One key needs your attention under Details."
                      : "\(review.pendingIdentityCount + review.missingApprovedKeyCount) keys need your attention under Details.",
                      systemImage: "exclamationmark.triangle.fill").foregroundStyle(.orange)
            }
            if narrowed {
                VStack(alignment: .leading, spacing: 8) {
                    Label("Review the available settings", systemImage: "exclamationmark.triangle")
                        .font(.headline)
                    if !review.blockedRouteIDs.isEmpty {
                        Text("\(review.blockedRouteIDs.count) routes retain required policy that is unavailable. Sending this workspace keeps those restrictions in place.")
                    }
                    ForEach(Array(sourceFindings.prefix(3).enumerated()), id: \.offset) { _, finding in
                        Text(verbatim: finding.reason)
                    }
                    if sourceFindings.count > 3 {
                        Text("\(sourceFindings.count - 3) more source findings are listed under Details and travel with the encrypted transfer.")
                    }
                    Text("Send Available Settings accepts this narrower workspace. Review the complete report under Details before sending.")
                        .foregroundStyle(.secondary)
                }.font(.callout).frame(maxWidth: .infinity, alignment: .leading)
            }
            HStack(spacing: 14) {
                Button {
                    model.sendToPhone()
                } label: {
                    Label(narrowed ? "Send Available Settings to iPhone" : "Send to iPhone", systemImage: "iphone.and.arrow.forward")
                        .font(.title3.weight(.semibold)).padding(.horizontal, 10).padding(.vertical, 4)
                }.buttonStyle(.borderedProminent).controlSize(.extraLarge).keyboardShortcut(.defaultAction)
                    .disabled(model.busy || !review.canPrepare || !model.destinationReady)
                if model.busy { ProgressView().controlSize(.small) }
            }
            Text("Your keys travel inside the animation. Anyone who films this screen while it plays could read them.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .padding(22).frame(maxWidth: .infinity, alignment: .leading)
        .background(.background.secondary, in: RoundedRectangle(cornerRadius: 18))
    }

    @ViewBuilder
    private func exportDetails(_ source: WorkspaceDocument) -> some View {
        GroupBox("Choose groups") {
            VStack(alignment: .leading, spacing: 12) {
                Text(verbatim: source.name).font(.headline)
                ForEach(source.groups) { group in
                    Toggle(isOn: Binding(get: { model.selectedGroupIDs.contains(group.id) },
                                         set: { model.selectGroup(group.id, included: $0) })) {
                        VStack(alignment: .leading, spacing: 3) {
                            Text(verbatim: group.name).font(.headline)
                            Text(verbatim: group.machineOrder.compactMap { id in source.machines.first(where: { $0.id == id })?.name }.joined(separator: " → "))
                                .font(.caption).foregroundStyle(.secondary).lineLimit(2)
                        }
                    }
                    .disabled(model.busy || group.machineOrder.isEmpty
                        || (model.selectedGroupIDs.contains(group.id) && !model.canDeselectGroup(group.id)))
                }
                if let selected = model.selection?.workspace {
                    Text("\(selected.groups.count) groups · \(selected.machines.count) machines · \(selected.routes.count) routes including jump hosts")
                        .font(.callout)
                    Text("Every selected machine’s route order, jump dependencies, host policy, palette, and connection preferences travel together.")
                        .font(.caption).foregroundStyle(.secondary)
                    Button("Save Selected Public Workspace…") { model.savePublicWorkspace() }.disabled(model.busy)
                    Text("A public .ftworkspace file contains no private keys and needs no transfer code. Hostnames and public trust are visible; verify the displayed file comparison value on the phone. Required source restrictions remain enforced.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if source.discardedExtensionCount > 0 {
                    Label("\(source.discardedExtensionCount) unrecognized extension payloads were excluded; any required policy markers remain enforced.", systemImage: "info.circle")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }.frame(maxWidth: .infinity, alignment: .leading).padding(10)
        }
        if let selection = model.selection, let review = model.exportReview {
            GroupBox("Review machines and host trust") {
                VStack(alignment: .leading, spacing: 14) {
                    Text("Machines in selected group order").font(.subheadline.bold())
                    ForEach(selection.orderedMachines) { machine in
                        machinePreview(machine, document: selection.workspace)
                    }
                    DisclosureGroup("Connection routes and host trust") {
                        VStack(alignment: .leading, spacing: 14) {
                            ForEach(selection.workspace.routes) { route in
                                routePreview(route, document: selection.workspace)
                            }
                        }.padding(.top, 8)
                    }
                }.frame(maxWidth: .infinity, alignment: .leading).padding(10)
            }
            GroupBox("Choose each private identity") {
                VStack(alignment: .leading, spacing: 16) {
                    if selection.identities.isEmpty { Text("These routes reference no private-key identities.") }
                    ForEach(selection.identities) { identity in
                        identityChoice(identity)
                    }
                    Text("One private record serves every selected reference to the same public key. Keeping a reference only removes any private material already loaded for that identity.")
                        .font(.caption).foregroundStyle(.secondary)
                    Text("Supported source keys: OpenSSH Ed25519, ECDSA P-256/P-384/P-521, and RSA (2048–8192 bits), plus ECDSA/RSA PEM files. Protected OpenSSH files use bcrypt with AES-CTR or AES-CBC; encrypted PKCS#8 files use PBES2, PBKDF2 and AES-CBC. Clear SEC1, PKCS#1 and PKCS#8 files are also supported. RSA connections require SHA-2 signatures. Explicit OpenSSH user certificates remain attached to their public references; including their matching private key is a separate choice. Hardware-bound keys cannot be copied into this transfer.")
                        .font(.caption).foregroundStyle(.secondary)
                }.frame(maxWidth: .infinity, alignment: .leading).padding(10)
            }
            destinationChoice
            GroupBox("Review the export") {
                VStack(alignment: .leading, spacing: 12) {
                    Text("\(review.includedIdentityCount) private identities included · \(review.omittedIdentityCount) references kept without private material")
                    if review.pendingIdentityCount > 0 {
                        Text("Choose whether to include or omit each of the \(review.pendingIdentityCount) remaining identities.").foregroundStyle(.orange)
                    }
                    if review.missingApprovedKeyCount > 0 {
                        Text("Choose matching key files for \(review.missingApprovedKeyCount) identities marked for inclusion.").foregroundStyle(.orange)
                    }
                    ForEach(Array(review.findings.enumerated()), id: \.offset) { _, finding in
                        VStack(alignment: .leading, spacing: 3) {
                            Label(finding.disposition == 1 ? "Required policy unavailable"
                                  : finding.setting == "credential_policy" ? "Credential policy preserved"
                                  : finding.setting == "ssh_config.source_scope" || finding.setting == MachineCompanionSource.scopeSetting
                                    || finding.setting == MachineCompanionSource.desktopScopeSetting
                                    ? "Source scope" : "Preserved with an omission",
                                  systemImage: finding.disposition == 1 ? "lock.trianglebadge.exclamationmark" : "info.circle")
                                .font(.caption.bold())
                            Text(verbatim: finding.entityID + ": " + finding.reason).font(.caption)
                        }
                    }
                    Text("The phone evaluates its existing identities, reviewed host trust, and transport support separately. Required policies remain enforced after restoring a narrower workspace.")
                        .font(.caption).foregroundStyle(.secondary)
                    Button("Clear Loaded Material", role: .destructive) { model.clear() }.disabled(model.busy)
                }.frame(maxWidth: .infinity, alignment: .leading).padding(10)
            }
        }
    }

    private var destinationChoice: some View {
        GroupBox("Choose how to open the transfer") {
            VStack(alignment: .leading, spacing: 12) {
                Picker("Protection", selection: Binding(get: { model.protection }, set: { model.selectProtection($0) })) {
                    Text("Generated Transfer Code").tag(ExportProtection.code)
                    Text("Only This iPhone").tag(ExportProtection.recipient)
                }.pickerStyle(.segmented).disabled(model.busy)
                if model.protection == .code {
                    Text("Open the package with a new private transfer code. Keep that code separately from the encrypted file or recording when saving a recovery backup.")
                        .font(.callout)
                } else {
                    Text("Scan the public request displayed by FrankenTerm on the intended iPhone. Confirm the recipient value on both screens before any private material is exported.")
                        .font(.callout)
                    HStack {
                        Button("Scan iPhone with Camera…") { model.scanRecipientWithCamera() }
                            .disabled(model.busy || model.requestingCameraAccess)
                        Button("Scan Local QR Image…") { model.scanRecipientImage() }
                            .disabled(model.busy || model.requestingCameraAccess)
                        if model.requestingCameraAccess {
                            ProgressView().controlSize(.small)
                            Button("Cancel") { model.cancelRecipientScan() }
                        }
                    }
                    if let request = model.recipientRequest, let comparison = model.recipientComparison {
                        let requestDigest = AppleCryptography().sha256(request.encoded)
                        Text("Recipient comparison").font(.caption.bold())
                        Text(verbatim: comparison).font(.system(.title3, design: .monospaced).bold()).textSelection(.enabled)
                        Text("The iPhone accepts at most \(request.maximumEnvelopeBytes) encrypted bytes for this request.")
                            .font(.caption).foregroundStyle(.secondary)
                        Toggle("I compared this recipient value directly with my iPhone and it matches.",
                               isOn: Binding(get: { model.confirmedRecipientDigest == requestDigest },
                                             set: { model.confirmRecipient($0, requestDigest: requestDigest) }))
                            .disabled(model.busy)
                    }
                    Text("Cancelling the iPhone’s receive session or losing its request key makes these encrypted files and videos unusable. For a recording that remains recoverable independently of this session, choose Generated Transfer Code.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }.frame(maxWidth: .infinity, alignment: .leading).padding(10)
        }
    }

    private func machinePreview(_ machine: WorkspaceDocument.Machine, document: WorkspaceDocument) -> some View {
        let theme = document.themes.first(where: { $0.id == machine.themeID })
        let preferred = document.routes.first(where: { $0.id == machine.preferredRouteID })
        return HStack(spacing: 10) {
            Circle().fill(theme.map { paletteColor($0.accent) } ?? Color.accentColor).frame(width: 12, height: 12).accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 3) {
                Text(verbatim: machine.name).font(.subheadline.bold())
                if let preferred {
                    Text("\(preferred.user)@\(preferred.host):\(preferred.port)").font(.caption).foregroundStyle(.secondary)
                }
            }
            Spacer()
            if let theme { Text(verbatim: theme.name).font(.caption).foregroundStyle(.secondary) }
            if machine.id == document.initialMachineID { Label("Selected first", systemImage: "arrow.right.circle").font(.caption) }
        }
    }

    private func routePreview(_ route: WorkspaceDocument.Route, document: WorkspaceDocument) -> some View {
        let trust = document.hostTrust.first(where: { $0.id == route.trustID })
        let related = trust.map { selected in document.hostTrust.filter { $0.canonicalTrustScope == selected.canonicalTrustScope } } ?? []
        let required = Set(route.requiredCapabilities).union(route.requiredExtensions.map { "extension:" + $0 })
            .union(related.flatMap(\.requiredPolicyRequirements))
        let revoked = Array(Set(related.flatMap(\.revokedKeys))).sorted {
            $0.algorithm == $1.algorithm ? $0.fingerprint < $1.fingerprint : $0.algorithm < $1.algorithm
        }
        return VStack(alignment: .leading, spacing: 4) {
            Text("\(route.user)@\(route.host):\(route.port)").font(.subheadline.bold())
            Text("\(route.mode == "direct_ssh" ? "Direct SSH" : "FrankenTerm mux") · \(route.jumpRoutes.count) configured jump routes · \(route.credentialIDs.count) credential candidates")
                .font(.caption).foregroundStyle(.secondary)
            Text("Permitted authentication: \(route.authenticationMethods.map(\.rawValue).joined(separator: ", "))")
                .font(.caption).foregroundStyle(.secondary)
            if !required.isEmpty {
                Label("Required policy: \(required.sorted().joined(separator: ", "))", systemImage: "exclamationmark.shield")
                    .font(.caption).foregroundStyle(.orange)
            }
            if let trust {
                if trust.pins.isEmpty { Label("Host verification required", systemImage: "questionmark.shield").font(.caption) }
                ForEach(trust.pins, id: \.self) { key in
                    Text("Pin · \(key.algorithm) · \(key.fingerprint)").font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                }
                ForEach(revoked, id: \.self) { key in
                    Text("Revoked · \(key.algorithm) · \(key.fingerprint)").font(.system(.caption, design: .monospaced)).foregroundStyle(.red).textSelection(.enabled)
                }
            }
        }.frame(maxWidth: .infinity, alignment: .leading)
    }

    private func identityChoice(_ identity: WorkspaceExportSelection.Identity) -> some View {
        let supported = ["ssh-ed25519", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521", "ssh-rsa"].contains(identity.algorithm)
        let certified = model.selection?.workspace.credentials.filter {
            $0.fingerprint == identity.fingerprint && $0.userCertificate != nil
        } ?? []
        return VStack(alignment: .leading, spacing: 8) {
            Text(verbatim: identity.credentialIDs.joined(separator: ", ")).font(.headline)
            Text(verbatim: identity.fingerprint).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
            Text(verbatim: algorithmName(identity.algorithm) + " · Routes: " + identity.routeIDs.joined(separator: ", "))
                .font(.caption).foregroundStyle(.secondary)
            ForEach(certified) { reference in
                if let certificate = reference.userCertificate {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(verbatim: "User certificate · " + certificate.fingerprint)
                            .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                        Text(verbatim: "Signing authority · " + certificate.authority.fingerprint)
                            .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                        Text(verbatim: "Principals · " + (certificate.principals.isEmpty ? "Any account accepted by the server" : certificate.principals.joined(separator: ", ")))
                        Text(verbatim: "Valid from " + certificateTime(certificate.validAfter) + " until " +
                            (certificate.validBefore == UInt64.max ? "no expiration" : certificateTime(certificate.validBefore)))
                    }.font(.caption).foregroundStyle(.secondary)
                }
            }
            if !supported {
                Text("Private-key import is unavailable for this algorithm. Keep its public reference for a matching identity on the phone.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            HStack {
                Picker("Private material", selection: Binding(
                    get: { model.identityDecisions[identity.fingerprint, default: .pending] },
                    set: { model.decide($0, fingerprint: identity.fingerprint) })) {
                    Text("Choose…").tag(WorkspaceExportSelection.IdentityDecision.pending)
                    Text("Include Private Key").tag(WorkspaceExportSelection.IdentityDecision.includePrivateKey).disabled(!supported)
                    Text("Keep Reference Only").tag(WorkspaceExportSelection.IdentityDecision.keepReferenceOnly)
                }.frame(maxWidth: 365).disabled(model.busy)
                Spacer()
                if model.includedFingerprints.contains(identity.fingerprint) {
                    Label("Key loaded", systemImage: "checkmark.shield").font(.caption)
                }
                if model.identityDecisions[identity.fingerprint] == .includePrivateKey, let credentialID = identity.credentialIDs.first {
                    if !model.includedFingerprints.contains(identity.fingerprint),
                       let configured = model.configuredKey(for: identity.fingerprint) {
                        Button("Use Key on This Mac…") { model.useConfiguredKey(configured) }
                            .help("Read this configured file and unlock it locally if needed: \(configured.fileReference)")
                            .disabled(model.busy || !supported)
                    }
                    Button("Choose Matching Key…") { model.chooseKey(credentialID) }.disabled(model.busy || !supported)
                }
            }
            if model.identityDecisions[identity.fingerprint] == .includePrivateKey,
               !model.includedFingerprints.contains(identity.fingerprint),
               let configured = model.configuredKey(for: identity.fingerprint) {
                Text(verbatim: "Configured identity file: " + configured.fileReference)
                    .font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                Text("Use this file to unlock the selected identity without browsing again. A .pub reference uses its matching private file; the key must still match this fingerprint.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    private func certificateTime(_ seconds: UInt64) -> String {
        guard seconds <= 253_402_300_799 else { return "beyond year 9999" }
        return Date(timeIntervalSince1970: Double(seconds)).formatted(date: .abbreviated, time: .shortened)
    }

    private func algorithmName(_ algorithm: String) -> String {
        switch algorithm {
        case "ssh-ed25519": return "Ed25519"
        case "ecdsa-sha2-nistp256": return "ECDSA P-256"
        case "ecdsa-sha2-nistp384": return "ECDSA P-384"
        case "ecdsa-sha2-nistp521": return "ECDSA P-521"
        case "ssh-rsa": return "RSA (SHA-2 signatures)"
        default: return algorithm
        }
    }

    private func paletteColor(_ hex: String) -> Color {
        guard let value = UInt32(hex.dropFirst(), radix: 16) else { return .accentColor }
        let alpha = hex.count == 9
        return Color(red: Double((value >> (alpha ? 24 : 16)) & 255) / 255,
                     green: Double((value >> (alpha ? 16 : 8)) & 255) / 255,
                     blue: Double((value >> (alpha ? 8 : 0)) & 255) / 255,
                     opacity: alpha ? Double(value & 255) / 255 : 1)
    }
}

/// The capture session and metadata delegate stay on one serial queue. Only a
/// bounded request string crosses to the main actor; frames are never recorded.
@MainActor
private struct RecipientRequestCamera: NSViewControllerRepresentable {
    let onRequest: @MainActor @Sendable (RecipientRequest) -> Void
    let onError: @MainActor @Sendable (RecipientRequestCameraError) -> Void

    func makeNSViewController(context: Context) -> NativeRecipientRequestScanner {
        NativeRecipientRequestScanner(onRequest: onRequest, onFailure: onError)
    }
    func updateNSViewController(_ controller: NativeRecipientRequestScanner, context: Context) {}
    static func dismantleNSViewController(_ controller: NativeRecipientRequestScanner, coordinator: ()) {
        controller.stop()
    }
}

@MainActor
private struct SourceKeyUnlockSheet: View {
    @Bindable var model: MacExportModel
    let request: SourceKeyUnlockRequest
    @State private var passphrase = ""
    @Environment(\.scenePhase) private var scenePhase

    private var canUnlock: Bool {
        !model.busy && model.sourceSheet?.id == request.id && model.keyUnlockRequest?.id == request.id &&
            (1...OpenSSHSourceKey.maximumPassphraseBytes).contains(passphrase.utf8.count)
    }
    private func submit() {
        guard canUnlock else { return }
        let secret = Data(passphrase.utf8)
        passphrase = ""
        model.unlockSourceKey(requestID: request.id, passphrase: secret)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Label("Unlock selected source key", systemImage: "key.fill").font(.title2.bold())
            Text(verbatim: request.credentialID).font(.headline)
            if request.protection.hidesPublicIdentity {
                Text("Expected workspace identity").font(.caption.bold())
            }
            Text(verbatim: request.algorithm + "\n" + request.fingerprint)
                .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
            if request.protection.hidesPublicIdentity {
                Text("This file keeps its public identity encrypted too. After unlocking, its key must match this algorithm and fingerprint before it can be included.")
                    .font(.callout)
            }
            Text("Enter this key file's passphrase. The key is checked locally and included only in the encrypted transfer you approve.")
                .font(.callout)
            SecureField("Source-key passphrase", text: $passphrase)
                .textContentType(.password).privacySensitive().disabled(model.busy)
                .onSubmit(submit)
            if let active = model.keyUnlockRequest, active.id == request.id {
                Text("\(active.attemptsRemaining) attempts remaining for this selected file.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let message = model.message { Text(verbatim: message).font(.callout).foregroundStyle(.red) }
            HStack {
                Button("Cancel") { passphrase = ""; model.cancelKeyUnlock(requestID: request.id) }
                    .keyboardShortcut(.cancelAction)
                Spacer()
                if model.busy { ProgressView().controlSize(.small) }
                Button("Unlock Key", action: submit).buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction).disabled(!canUnlock)
            }
        }.padding(24).frame(width: 510)
            .onDisappear { passphrase = "" }
            .onChange(of: scenePhase, initial: true) { _, phase in
                if phase != .active { passphrase = ""; model.cancelKeyUnlock(requestID: request.id) }
            }
    }
}

@MainActor
private struct PixelPerfectQRCode: NSViewRepresentable {
    let symbol: QRCodeSymbol
    func makeNSView(context: Context) -> QRView { QRView() }
    func updateNSView(_ view: QRView, context: Context) { view.set(symbol) }
    @MainActor
    final class QRView: NSView {
        private var image: CGImage?
        private var digest: Data?
        func set(_ symbol: QRCodeSymbol) {
            guard digest != symbol.png else { return }
            digest = symbol.png
            if let source = CGImageSourceCreateWithData(symbol.png as CFData, nil) { image = CGImageSourceCreateImageAtIndex(source, 0, nil) }
            else { image = nil }
            needsDisplay = true
        }
        override func viewDidChangeBackingProperties() { super.viewDidChangeBackingProperties(); needsDisplay = true }
        override func draw(_ dirtyRect: NSRect) {
            NSColor.white.setFill(); bounds.fill()
            guard let image, let context = NSGraphicsContext.current?.cgContext else { return }
            let backing = window?.backingScaleFactor ?? 1
            let scale = floor(min(bounds.width, bounds.height) * backing / CGFloat(image.width))
            guard scale >= 1 else { return }
            let size = CGFloat(image.width) * scale / backing
            let x = floor((bounds.width - size) * backing / 2) / backing
            let y = floor((bounds.height - size) * backing / 2) / backing
            context.interpolationQuality = .none; context.setShouldAntialias(false)
            context.draw(image, in: CGRect(x: x, y: y, width: size, height: size))
        }
    }
}

private extension Color {
    /// A theme color from a workspace document (`#RRGGBB` or `#RRGGBBAA`).
    init(ftExportHex hex: String) {
        let raw = String(hex.dropFirst())
        let number = UInt64(raw, radix: 16) ?? 0
        let rgba = raw.count == 8 ? number : (number << 8) | 255
        self.init(.sRGB, red: Double((rgba >> 24) & 255) / 255, green: Double((rgba >> 16) & 255) / 255,
                  blue: Double((rgba >> 8) & 255) / 255, opacity: Double(rgba & 255) / 255)
    }
}
