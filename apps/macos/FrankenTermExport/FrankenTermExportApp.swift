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

@main
@MainActor
struct FrankenTermExportApp: App {
    @State private var model = MacExportModel()
    var body: some Scene {
        Window("FrankenTerm Offline Export", id: "export") {
            MacExportView(model: model)
        }.defaultSize(width: 850, height: 900)
    }
}

/// File reads, parsing, key validation, compression and encryption remain off UI.
/// SSH source selection reads only the chosen config, its referenced public
/// identities, and the user's known_hosts. Private files require a separate pick.
private actor MacExportEngine {
    struct Prepared: Sendable {
        let comparison: String, pages: Int, bytes: Int
        let protection: ExportProtection
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
    private var sourceFindings: [AirGapArchive.Portability] = []
    private var identityReferences: [String: String] = [:]
    private var sshSource: (source: SSHWorkspaceSource, url: URL)?
    private var decisions: [String: WorkspaceExportSelection.IdentityDecision] = [:]
    private var keys: [PrivateKeyRecord] = []
    private var exportedEnvelope: Data?
    private var transferCode: TransferCode?
    private var sender: OpticalSender?

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
    func invalidateExport() { exportedEnvelope = nil; transferCode = nil; sender = nil }
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
        sourceFindings = []; identityReferences = [:]; sshSource = nil
        return try state()
    }
    func inspectSSHConfiguration(_ url: URL) throws -> [String] {
        retireKeyRequest(); invalidateExport()
        sshSource = nil
        let source = try SSHWorkspaceSource(configuration: SelectedLocalFile.read(url, limit: SSHWorkspaceSource.maximumSourceBytes))
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
        var publicKeys: [String: Data] = [:]
        for reference in try pending.source.identityFileReferences(for: aliases) {
            try Task.checkCancellation()
            guard let identity = Self.identityURL(reference) else { continue }
            let url = identity.pathExtension == "pub" ? identity : identity.appendingPathExtension("pub")
            do { publicKeys[reference] = try SelectedLocalFile.read(url, limit: FTLimit.key) }
            catch is CancellationError { throw CancellationError() }
            catch { /* The source reports missing public identities explicitly. */ }
        }
        let knownHostsURL = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".ssh/known_hosts")
        let knownHosts: Data?
        do { knownHosts = try SelectedLocalFile.read(knownHostsURL, limit: SSHWorkspaceSource.maximumSourceBytes) }
        catch is CancellationError { throw CancellationError() }
        catch {
            let code = (error as? CocoaError)?.code
            if code == .fileNoSuchFile || code == .fileReadNoSuchFile || (error as? POSIXError)?.code == .ENOENT {
                knownHosts = nil
            } else {
                // An unreadable revocation source is not equivalent to no file.
                throw FTError.unsupported("ssh.known_hosts_unreadable")
            }
        }
        let support = try FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
                                                  appropriateFor: nil, create: true)
        let sourceID = crypto.sha256(Data(pending.url.standardizedFileURL.path.utf8)).map { String(format: "%02x", $0) }.joined()
        let storage = try FileWorkspacePersistence(directory: support.appendingPathComponent("FrankenTermExport/ssh-" + sourceID))
        let (candidate, selected): (SSHWorkspaceExport, WorkspaceExportSelection) = try storage.withExclusiveAccess {
            var registry = try storage.read(.catalog, limit: FTLimit.manifest).map {
                try JSONDecoder().decode(SSHWorkspaceRegistry.self, from: $0)
            } ?? SSHWorkspaceRegistry()
            let result = try pending.source.export(aliases: aliases, workspaceName: "SSH Machines", defaultUser: NSUserName(),
                publicKeys: publicKeys, knownHosts: knownHosts, registry: &registry, using: crypto)
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
        identityReferences = candidate.identityReferences; sshSource = nil
        decisions = [:]; keys = []; invalidateExport()
        return try state()
    }
    func selectGroups(_ ids: Set<String>) throws -> SelectionState {
        retireKeyRequest(); invalidateExport()
        guard let document else { throw FTError.stale }
        let selected = try WorkspaceExportSelection(workspace: document, groupIDs: ids, findings: sourceFindings)
        let retained = Set(selected.identities.map(\.fingerprint))
        // A group change removes private material no longer referenced anywhere
        // in the selected dependency graph. It also invalidates the sealed export.
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
        let selected = try keyUnlocker.select(pem: SelectedLocalFile.read(url, limit: FTLimit.key), reference: reference)
        do { try Task.checkCancellation() }
        catch { retireKeyRequest(); throw error }
        switch selected {
        case let .ready(key):
            return .loaded(try installKey(key, requestID: nil))
        case let .passphraseRequired(request): return .passphraseRequired(request)
        }
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
    func prepare(recipient: RecipientRequest?) throws -> Prepared {
        retireKeyRequest(); invalidateExport()
        try Task.checkCancellation()
        guard let selection else { throw FTError.stale }
        // Each new seal is a distinct immutable export, including when a user
        // changes selected keys or prepares the same source document again.
        // Playback and file/animation saves continue reusing this one envelope.
        let archive = try selection.archive(decisions: decisions, keys: keys, bundleID: UUID().uuidString, using: crypto)
        let envelope: Data
        let code: TransferCode?
        if let recipient {
            envelope = try FTAG.seal(archive, recipient: recipient, using: crypto)
            code = nil
        } else {
            let candidate = try FTAG.seal(archive, using: crypto)
            envelope = candidate.envelope; code = candidate.code
        }
        try Task.checkCancellation()
        let optical = try OpticalSender(envelope: envelope, using: crypto)
        exportedEnvelope = envelope; transferCode = code; sender = optical
        return .init(comparison: FTAG.comparison(envelope, using: crypto), pages: optical.identity.pageCount,
                     bytes: envelope.count, protection: recipient == nil ? .code : .recipient)
    }
    func next(repairPage: Int?) throws -> String {
        guard var active = sender else { throw FTError.stale }
        let text = try active.next(repairPage: repairPage, using: crypto)
        sender = active
        return text
    }
    func code() throws -> String {
        guard exportedEnvelope != nil, let transferCode else { throw FTError.stale }
        return transferCode.reveal(using: crypto)
    }
    func encryptedEnvelope() throws -> Data {
        guard let exportedEnvelope else { throw FTError.stale }
        return exportedEnvelope
    }
    func saveEncryptedFile(to url: URL) throws {
        guard let exportedEnvelope else { throw FTError.stale }
        try save(exportedEnvelope, to: url)
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
        // Deliberate new-file save only. Never overwrite another backup silently.
        try bytes.write(to: url, options: .withoutOverwriting)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
        let handle = try FileHandle(forWritingTo: url)
        try handle.synchronize(); try handle.close()
    }
    func clear() {
        retireKeyRequest(); document = nil; selection = nil; decisions = [:]; keys = []
        sourceFindings = []; identityReferences = [:]; sshSource = nil; invalidateExport()
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
    var symbol: QRCodeSymbol?
    var revealedCode: String?
    var message: String?
    var busy = false
    var playing = false
    var emitted = 0
    var rate = 5
    var repairPage = -1
    var approveSelectedKeys = false
    private(set) var protection = ExportProtection.code
    private(set) var recipientRequest: RecipientRequest?
    private(set) var recipientComparison: String?
    private(set) var recipientConfirmed = false
    private(set) var scanningRecipient = false
    private(set) var requestingCameraAccess = false
    private(set) var recipientScanID = UUID()
    @ObservationIgnored private let engine = MacExportEngine()
    @ObservationIgnored private let renderer = NativeOpticalPresentation()
    @ObservationIgnored private let videoRenderer = NativeOpticalVideo()
    @ObservationIgnored private var playback: Task<Void, Never>?
    @ObservationIgnored private var work: Task<Void, Never>?
    @ObservationIgnored private var generation = UUID()
    @ObservationIgnored private var awake: NSObjectProtocol?
    @ObservationIgnored private var foreground = true
    @ObservationIgnored private var cameraPermission: Task<Void, Never>?
    @ObservationIgnored private var sourceKeyOperation: UUID?
    @ObservationIgnored private var sshSelectionID: UUID?

    var selectedGroupIDs: Set<String> { Set(selection?.workspace.groups.map(\.id) ?? []) }
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
        pause(); revealedCode = nil; prepared = nil; approveSelectedKeys = false; emitted = 0; repairPage = -1
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
        protection = value; recipientRequest = nil; recipientComparison = nil; recipientConfirmed = false
        perform { [self] in await engine.invalidateExport() }
    }
    func confirmRecipient(_ value: Bool) {
        guard !busy, sourceSheet == nil, sourceKeyOperation == nil, protection == .recipient, recipientRequest != nil else { return }
        // Approval belongs to this exact parsed request. Replacing it, changing
        // modes, or withdrawing confirmation also invalidates the sealed export.
        invalidatePrepared(); recipientConfirmed = value
        perform { [self] in await engine.invalidateExport() }
    }
    private func resetRecipient() {
        cancelRecipientScan(); invalidatePrepared()
        recipientRequest = nil; recipientComparison = nil; recipientConfirmed = false
    }
    private func acceptRecipient(_ request: RecipientRequest) {
        recipientRequest = request; recipientComparison = request.comparison(using: AppleCryptography())
        recipientConfirmed = false; approveSelectedKeys = false
        message = "Compare the recipient value directly with the iPhone before approving this destination."
    }
    private func presentRecipientCamera(scanID: UUID) {
        guard foreground, protection == .recipient, recipientScanID == scanID,
              sourceSheet == nil, sourceKeyOperation == nil, sshSelectionID == nil else { return }
        scanningRecipient = true; sourceSheet = .recipientCamera(scanID)
    }
    func scanRecipientWithCamera() {
        guard !busy, foreground, sourceSheet == nil, sourceKeyOperation == nil,
              protection == .recipient, !requestingCameraAccess else { return }
        resetRecipient()
        let token = recipientScanID
        perform { [self] in
            await engine.invalidateExport()
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
                guard !Task.isCancelled, self.foreground, self.protection == .recipient else { return }
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
    func receiveRecipient(_ text: String, scanID: UUID) {
        guard foreground, !busy, protection == .recipient, scanningRecipient,
              sourceSheet?.id == scanID, recipientScanID == scanID else { return }
        cancelRecipientScan(scanID: scanID)
        do { acceptRecipient(try RecipientRequest(text: text)) }
        catch { report(error) }
    }
    func recipientCameraFailed(_ error: FTError, scanID: UUID) {
        guard scanningRecipient, sourceSheet?.id == scanID, recipientScanID == scanID else { return }
        cancelRecipientScan(scanID: scanID); report(error)
    }
    func scanRecipientImage() {
        guard !busy, sourceSheet == nil, sourceKeyOperation == nil, protection == .recipient else { return }
        // A delayed camera-permission callback must not present over a file panel.
        cancelRecipientScan()
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.png, .jpeg, .heic]
        panel.message = "Choose a local image containing only the iPhone’s FrankenTerm recipient-request QR. You must compare its recipient value with the phone before exporting."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        resetRecipient()
        let token = recipientScanID
        perform { [self] in
            let request = try await engine.scanRecipientImage(url)
            try Task.checkCancellation()
            guard foreground, protection == .recipient, recipientScanID == token else { return }
            acceptRecipient(request)
        }
    }

    func chooseWorkspace() {
        guard !busy, sourceSheet == nil else { return }
        cancelRecipientScan()
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.json, .data]; panel.message = "Choose a local .ftworkspace JSON document. Executable Mac configuration is never evaluated."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        retireSourceKeyPrompt(); retireSSHSelection(); invalidatePrepared()
        perform { [self] in
            apply(try await engine.loadWorkspace(url))
        }
    }
    func chooseSSHConfiguration() {
        guard !busy, sourceSheet == nil else { return }
        cancelRecipientScan()
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.showsHiddenFiles = true
        panel.directoryURL = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".ssh")
        panel.message = "Choose an SSH config, then select its hosts. This reads matching public .pub files and ~/.ssh/known_hosts. Private keys are chosen separately; commands and Include files are not evaluated."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        retireSourceKeyPrompt(); retireSSHSelection(); invalidatePrepared()
        perform { [self] in
            let aliases = try await engine.inspectSSHConfiguration(url)
            do { try Task.checkCancellation() }
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
        perform { [self] in
            apply(try await engine.loadSSHSelection(aliases))
            retireSSHSelection()
        }
    }
    func dismissSSHSelection(id: UUID? = nil) {
        guard !busy, let current = sshSelectionID, id == nil || id == current else { return }
        cancelRecipientScan(); retireSourceKeyPrompt(); retireSSHSelection()
        perform { [self] in await engine.cancelSSHSelection() }
    }
    func selectGroup(_ id: String, included: Bool) {
        guard !busy, sourceSheet == nil else { return }
        var groups = selectedGroupIDs
        if included { groups.insert(id) } else { groups.remove(id) }
        guard !groups.isEmpty else { return }
        cancelRecipientScan(); retireSourceKeyPrompt(); invalidatePrepared()
        perform { [self] in apply(try await engine.selectGroups(groups)) }
    }
    func decide(_ value: WorkspaceExportSelection.IdentityDecision, fingerprint: String) {
        guard !busy, sourceSheet == nil else { return }
        cancelRecipientScan(); retireSourceKeyPrompt(); invalidatePrepared()
        perform { [self] in apply(try await engine.decide(value, fingerprint: fingerprint)) }
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
        panel.message = "Choose the matching OpenSSH Ed25519 or ECDSA key (P-256, P-384, or P-521). Protected files using bcrypt and AES-CTR are unlocked locally after you enter their passphrase."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        retireSourceKeyPrompt(); retireSSHSelection(); invalidatePrepared()
        let token = UUID(); sourceKeyOperation = token
        perform { [self] in
            do {
                let result = try await engine.loadKey(url, credentialID: credentialID, operationID: token)
                try Task.checkCancellation()
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
        perform { [self] in
            var secret = passphrase
            defer { secret.resetBytes(in: 0..<secret.count) }
            do {
                let result = try await engine.unlockKey(operationID: token, requestID: requestID, passphrase: secret)
                try Task.checkCancellation()
                guard foreground, sourceSheet?.id == requestID, sourceKeyOperation == token,
                      keyUnlockRequest?.id == requestID else { throw CancellationError() }
                retireSourceKeyPrompt(); apply(result)
                await engine.acknowledgeKey(operationID: token)
            } catch {
                let remaining = await engine.pendingKeyRequest(operationID: token)
                if !Task.isCancelled, foreground, sourceSheet?.id == requestID,
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
    func prepare() {
        guard !busy, foreground, sourceSheet == nil, sourceKeyOperation == nil,
              approveSelectedKeys, exportReview?.canPrepare == true, destinationReady else { return }
        let recipient = protection == .recipient ? recipientRequest : nil
        retireSourceKeyPrompt()
        pause(); revealedCode = nil; prepared = nil; emitted = 0; repairPage = -1
        perform { [self] in
            let result = try await engine.prepare(recipient: recipient)
            try Task.checkCancellation()
            prepared = result; emitted = 0; repairPage = -1
        }
    }
    private func perform(_ body: @escaping @MainActor () async throws -> Void) {
        guard !busy else { return }
        busy = true; message = nil
        work = Task {
            defer { busy = false; work = nil }
            do { try await body() } catch { report(error) }
        }
    }
    func play() {
        guard !busy, prepared != nil, foreground else { return }
        pause(); revealedCode = nil; playing = true
        let token = generation
        awake = ProcessInfo.processInfo.beginActivity(options: [.idleDisplaySleepDisabled], reason: "Display encrypted workspace QR transfer")
        playback = Task { [weak self] in
            while !Task.isCancelled {
                guard let self, self.playing, self.generation == token, self.foreground else { break }
                do {
                    let text = try await self.engine.next(repairPage: self.repairPage < 0 ? nil : self.repairPage)
                    let image = try await self.renderer.symbol(text: text)
                    guard !Task.isCancelled, self.playing, self.generation == token, self.foreground else { break }
                    self.symbol = image; self.emitted += 1
                    try await Task.sleep(for: .milliseconds(1000 / self.rate))
                } catch is CancellationError { break }
                catch { self.pause(); self.report(error); break }
            }
        }
    }
    func pause() {
        generation = UUID(); playback?.cancel(); playback = nil; playing = false; symbol = nil
        if let awake { ProcessInfo.processInfo.endActivity(awake); self.awake = nil }
    }
    func revealCode() {
        guard !busy, prepared?.protection == .code, foreground else { return }
        // Remove the QR and invalidate in-flight raster work BEFORE requesting code.
        pause(); revealedCode = nil
        let token = generation
        perform { [self] in
            let value = try await engine.code()
            guard foreground, generation == token else { return }
            revealedCode = value
        }
    }
    func concealCode() { revealedCode = nil }
    func setForeground(_ value: Bool) {
        foreground = value
        if !value {
            pause(); revealedCode = nil; cancelRecipientScan()
            if let token = sourceKeyOperation {
                work?.cancel(); retireSourceKeyPrompt()
                // The bounded worker checks cancellation before installing its
                // result; retire the selected source once that worker returns.
                Task { await engine.cancelKeyUnlock(operationID: token) }
            }
        }
    }
    func saveEncryptedFile() {
        guard !busy, prepared != nil else { return }
        pause(); revealedCode = nil
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-Workspace.ftairgap"
        let recipientOnly = prepared?.protection == .recipient
        panel.message = recipientOnly
            ? "Save encrypted bytes for the confirmed iPhone. This file will not open after the phone loses its active request key. Choose a new filename."
            : "Save encrypted bytes only. Keep the separately revealed transfer code in a different place. Choose a new filename."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        perform { [self] in
            try await engine.saveEncryptedFile(to: url)
            message = recipientOnly
                ? "Encrypted workspace saved for the confirmed iPhone. Its active request key is required; this file is not an independent recovery backup."
                : "Encrypted workspace saved. The transfer code is not included in this file."
        }
    }
    func savePublicWorkspace() {
        guard !busy, sourceSheet == nil, selection != nil else { return }
        cancelRecipientScan()
        pause(); revealedCode = nil
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-Workspace.ftworkspace"
        panel.message = "Save selected hosts, groups, public identities, and trust policy without private keys. The document is unencrypted. Check its file comparison value on your phone before importing. Choose a new filename."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        perform { [self] in
            let comparison = try await engine.savePublicWorkspace(to: url)
            message = "Public workspace saved without private keys. Compare this exact value on your phone through a trusted separate channel: " + comparison
        }
    }
    func saveAnimation() {
        guard !busy, prepared != nil else { return }
        pause(); revealedCode = nil
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-QR.apng"
        panel.allowedContentTypes = [.png]; panel.allowsOtherFileTypes = true
        panel.message = "Save a lossless animated PNG. Every encoded frame is checked before the final file is published. Choose a new filename."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let speed = rate, recipientOnly = prepared?.protection == .recipient
        perform { [self] in
            let bytes = try await engine.encryptedEnvelope()
            let count = try await renderer.saveAnimation(envelope: bytes, to: url, symbolsPerSecond: speed)
            message = "Saved and verified \(count) encrypted QR symbols. Play the APNG without interpolation. "
                + (recipientOnly ? "Only the confirmed iPhone’s active request key can open this recording." : "Keep its transfer code separate.")
        }
    }
    func saveVideo() {
        guard !busy, prepared != nil else { return }
        pause(); revealedCode = nil
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-QR.mp4"
        panel.allowedContentTypes = [.mpeg4Movie]
        panel.message = "Save an encrypted QR video. Every decoded frame and the reconstructed package must pass verification before the new file is published."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let speed = rate, recipientOnly = prepared?.protection == .recipient
        perform { [self] in
            let source = try OpticalVideoSource(envelope: await engine.encryptedEnvelope())
            let receipt = try await videoRenderer.save(source: source, to: url, symbolsPerSecond: speed)
            let duration = String(format: "%.1f", receipt.durationSeconds)
            message = "Saved and verified \(receipt.symbolCount) encrypted QR symbols in a \(duration)-second MP4 (\(receipt.frameCount) frames). "
                + (recipientOnly ? "The confirmed iPhone must retain its active request key; this is not an independent recovery backup." : "Keep its transfer code separate.")
        }
    }
    func cancelWork() {
        work?.cancel()
        if let token = sourceKeyOperation {
            retireSourceKeyPrompt()
            Task { await engine.cancelKeyUnlock(operationID: token) }
        }
    }
    func clear() {
        guard !busy else { return }
        retireSourceKeyPrompt(); retireSSHSelection(); resetRecipient(); protection = .code
        document = nil; selection = nil; identityDecisions = [:]; exportReview = nil
        includedFingerprints = []; prepared = nil; approveSelectedKeys = false; repairPage = -1
        identityReferences = [:]
        perform { [self] in await engine.clear() }
    }
    private func report(_ error: Error) {
        if let error = error as? OpenSSHSourceKeyError {
            message = error.localizedDescription
        } else if let error = error as? FTError {
            if error == .limit("source_key.kdf_parameters") || error == .limit("key.source_kdf") {
                message = "This key's protection settings exceed the supported unlock limits (bcrypt rounds 1–64 and salt 16–64 bytes). Choose another matching supported source file or keep its public reference."
            } else if error == .unsupported("source_key.cipher") || error == .unsupported("source_key.kdf") {
                message = "This protected key uses an unsupported format. Source unlocking supports OpenSSH bcrypt with AES-128-CTR or AES-256-CTR."
            } else if error == .limit("source_key.passphrase") {
                message = "Enter a passphrase between 1 and 1,024 UTF-8 bytes."
            } else if error == .limit("animation.use_live_transfer") {
                message = "This package exceeds the 256-symbol saved-animation limit. Use live QR playback or save the encrypted .ftairgap file. No settings or keys were omitted."
            } else if error == .limit("video.use_live_transfer") {
                message = "This package exceeds the bounded video duration or symbol count. Select fewer groups or identities, use live playback, or save the encrypted .ftairgap file. The package was not truncated."
            } else if error == .unsupported("video.published_durability_unconfirmed") {
                message = "The MP4 was verified and saved. The filesystem could not confirm that the directory change would survive a sudden system failure. The completed movie is retained at the selected destination."
            } else if error == .unsupported("ssh.no_concrete_hosts") {
                message = "This config has no concrete Host aliases to select. Wildcard-only definitions need named hosts in the selected file; included files are not evaluated."
            } else if error == .unsupported("ssh.known_hosts_unreadable") {
                message = "The public ~/.ssh/known_hosts file could not be read safely. Check its access and size before exporting so host revocations are not silently lost."
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
                message = "This export exceeds the size accepted by the iPhone’s request. Select fewer groups or private identities, or create and confirm a new request with a larger limit. Nothing was omitted automatically."
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
    @Environment(\.scenePhase) private var scenePhase
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 20) {
                HStack {
                    Label("FrankenTerm", systemImage: "terminal.fill").font(.largeTitle.bold())
                    Spacer()
                    Button("Import SSH Config…") { model.chooseSSHConfiguration() }.disabled(model.busy)
                    Button("Open Workspace…") { model.chooseWorkspace() }.disabled(model.busy)
                }
                Text("Offline workspace transfer").font(.title2).foregroundStyle(.secondary)
                if let document = model.document {
                    exportChoices(document)
                } else {
                    ContentUnavailableView("Choose a workspace", systemImage: "rectangle.3.group",
                        description: Text("Open a local .ftworkspace document, or import selected hosts from an SSH config with their public identities and known_hosts policy. Private keys always require a separate choice."))
                }
                if let prepared = model.prepared {
                    HStack {
                        VStack(alignment: .leading, spacing: 4) {
                            Text("Package comparison").font(.caption)
                            Text(verbatim: prepared.comparison).font(.system(.title3, design: .monospaced).bold())
                        }
                        Spacer()
                        Text("\(prepared.pages) pages · \(prepared.bytes) encrypted bytes").font(.caption)
                    }
                    Text("Compare this package value with the completed transfer on the iPhone, then review its source and contents before activating the workspace.")
                        .font(.caption).foregroundStyle(.secondary)
                    ZStack {
                        RoundedRectangle(cornerRadius: 20).fill(.white)
                        if let code = model.revealedCode {
                            VStack(spacing: 18) {
                                Label("Private transfer code", systemImage: "key.fill").font(.headline)
                                Text(verbatim: code).font(.system(.title2, design: .monospaced).bold()).textSelection(.disabled)
                                Text("Enter on the phone. Do not record this together with the QR animation.").multilineTextAlignment(.center)
                                Button("Hide Code") { model.concealCode() }
                            }.foregroundStyle(.black).padding(30)
                        } else if let symbol = model.symbol {
                            PixelPerfectQRCode(symbol: symbol).padding(16)
                        } else {
                            VStack(spacing: 12) {
                                Image(systemName: "qrcode").font(.system(size: 70))
                                Text("Animation paused").font(.headline)
                                Text("Scan using FrankenTerm’s Restore from Animated QR on your iPhone.").font(.callout)
                            }.foregroundStyle(.black).padding(24)
                        }
                    }.frame(height: 470).overlay { RoundedRectangle(cornerRadius: 20).stroke(.gray.opacity(0.3)) }
                    HStack {
                        Button(model.playing ? "Pause" : "Play QR Animation") { if model.playing { model.pause() } else { model.play() } }
                            .buttonStyle(.borderedProminent).disabled(model.busy)
                        if prepared.protection == .code {
                            Button("Reveal Code Separately") { model.revealCode() }.disabled(model.busy)
                        } else {
                            Label("Only the confirmed iPhone", systemImage: "iphone.and.arrow.forward").font(.caption)
                        }
                        Picker("Symbols/second", selection: $model.rate) { Text("3").tag(3); Text("5").tag(5); Text("10").tag(10) }.frame(width: 155)
                    }
                    HStack {
                        Picker("Repair", selection: $model.repairPage) {
                            Text("All pages").tag(-1)
                            ForEach(0..<prepared.pages, id: \.self) { Text("Page \($0 + 1)").tag($0) }
                        }.frame(width: 210)
                        Spacer()
                        Text("\(model.emitted) symbols emitted; receiver progress is shown on the phone.").font(.caption)
                    }
                    HStack {
                        Button("Save Encrypted File…") { model.saveEncryptedFile() }.disabled(model.busy)
                        Button("Save Verified MP4…") { model.saveVideo() }.disabled(model.busy)
                        Button("Save Verified QR Animation…") { model.saveAnimation() }.disabled(model.busy)
                        if model.busy { ProgressView().controlSize(.small); Button("Cancel Work") { model.cancelWork() } }
                    }
                    Text("MP4 video and lossless APNG animation reuse this package’s encrypted bytes. Each saved recording has a finite repair set; live playback can generate more. "
                         + (prepared.protection == .recipient
                            ? "No transfer code is generated. The iPhone must retain its active request key to open any saved file or recording."
                            : "The transfer code is never included in exported media."))
                        .font(.caption).foregroundStyle(.secondary)
                }
                if let message = model.message { Text(verbatim: message).font(.callout).textSelection(.enabled) }
            }.padding(28)
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
            .onChange(of: scenePhase, initial: true) { _, phase in model.setForeground(phase == .active) }
            .onDisappear { model.setForeground(false) }
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
            Text("Select up to 128 named hosts. Matching public .pub files and ~/.ssh/known_hosts supply public identities and host policy. Commands, Include files, and Match conditions remain unresolved and are reported before export.")
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
                    .disabled(model.busy || group.machineOrder.isEmpty || (model.selectedGroupIDs.count == 1 && model.selectedGroupIDs.contains(group.id)))
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
                    Text("Supported source keys: OpenSSH Ed25519 and ECDSA P-256, P-384, or P-521, including passphrase protection using bcrypt and AES-CTR. RSA, certificates, hardware keys, and capture from the running Mac workspace need additional support.")
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
                                  : finding.setting == "ssh_config.source_scope" ? "Source scope" : "Preserved with an omission",
                                  systemImage: finding.disposition == 1 ? "lock.trianglebadge.exclamationmark" : "info.circle")
                                .font(.caption.bold())
                            Text(verbatim: finding.entityID + ": " + finding.reason).font(.caption)
                        }
                    }
                    Text("The phone evaluates its existing identities, reviewed host trust, and transport support separately. Required policies remain enforced after restoring a narrower workspace.")
                        .font(.caption).foregroundStyle(.secondary)
                    Toggle("I approve these selected groups, \(review.includedIdentityCount) private identities, and the omissions shown above.", isOn: $model.approveSelectedKeys)
                        .disabled(model.busy || !review.canPrepare || !model.destinationReady)
                    HStack {
                        Button("Prepare Encrypted Transfer") { model.prepare() }.buttonStyle(.borderedProminent)
                            .disabled(model.busy || !model.approveSelectedKeys || !review.canPrepare || !model.destinationReady)
                        Button("Clear Loaded Material", role: .destructive) { model.clear() }.disabled(model.busy)
                        if model.busy { ProgressView().controlSize(.small) }
                    }
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
                        Text("Recipient comparison").font(.caption.bold())
                        Text(verbatim: comparison).font(.system(.title3, design: .monospaced).bold()).textSelection(.enabled)
                        Text("The iPhone accepts at most \(request.maximumEnvelopeBytes) encrypted bytes for this request.")
                            .font(.caption).foregroundStyle(.secondary)
                        Toggle("I compared this recipient value directly with my iPhone and it matches.",
                               isOn: Binding(get: { model.recipientConfirmed }, set: { model.confirmRecipient($0) }))
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
        let supported = ["ssh-ed25519", "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521"].contains(identity.algorithm)
        return VStack(alignment: .leading, spacing: 8) {
            Text(verbatim: identity.credentialIDs.joined(separator: ", ")).font(.headline)
            Text(verbatim: identity.fingerprint).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
            Text(verbatim: algorithmName(identity.algorithm) + " · Routes: " + identity.routeIDs.joined(separator: ", "))
                .font(.caption).foregroundStyle(.secondary)
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
                    Button("Choose Matching Key…") { model.chooseKey(credentialID) }.disabled(model.busy || !supported)
                }
            }
        }
    }

    private func algorithmName(_ algorithm: String) -> String {
        switch algorithm {
        case "ssh-ed25519": return "Ed25519"
        case "ecdsa-sha2-nistp256": return "ECDSA P-256"
        case "ecdsa-sha2-nistp384": return "ECDSA P-384"
        case "ecdsa-sha2-nistp521": return "ECDSA P-521"
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
private struct RecipientRequestCamera: NSViewRepresentable {
    let onRequest: @MainActor @Sendable (String) -> Void
    let onError: @MainActor @Sendable (FTError) -> Void

    func makeCoordinator() -> Capture { Capture(onRequest: onRequest, onError: onError) }
    func makeNSView(context: Context) -> Preview {
        let view = Preview()
        view.preview.session = context.coordinator.session
        context.coordinator.start()
        return view
    }
    func updateNSView(_ view: Preview, context: Context) {}
    static func dismantleNSView(_ view: Preview, coordinator: Capture) {
        coordinator.stop(); view.preview.session = nil
    }

    @MainActor
    final class Preview: NSView {
        let preview = AVCaptureVideoPreviewLayer()
        override init(frame frameRect: NSRect) {
            super.init(frame: frameRect)
            wantsLayer = true; layer = preview; preview.videoGravity = .resizeAspectFill
        }
        required init?(coder: NSCoder) { return nil }
        override func layout() { super.layout(); preview.frame = bounds }
    }

    /// AVFoundation's capture APIs predate Swift actor isolation. All mutable
    /// session state below is queue-owned; the cancellation bit alone is locked
    /// so dismantling a view invalidates a queued callback synchronously. The
    /// preview layer is attached/detached only by the main-actor view above.
    final class Capture: NSObject, AVCaptureMetadataOutputObjectsDelegate, @unchecked Sendable {
        let session = AVCaptureSession()
        private let queue = DispatchQueue(label: "org.frankenterm.recipient-camera", qos: .userInitiated)
        private let cancellation = NSLock()
        private var cancelled = false
        private var ended = false
        private var metadata: AVCaptureMetadataOutput?
        private var observers: [NSObjectProtocol] = []
        private let onRequest: @MainActor @Sendable (String) -> Void
        private let onError: @MainActor @Sendable (FTError) -> Void

        init(onRequest: @escaping @MainActor @Sendable (String) -> Void,
             onError: @escaping @MainActor @Sendable (FTError) -> Void) {
            self.onRequest = onRequest; self.onError = onError
            super.init()
        }
        private var isCancelled: Bool { cancellation.withLock { cancelled } }
        func start() {
            queue.async { [self] in
                guard !isCancelled, !ended else { return }
                do {
                    try configure()
                    guard !isCancelled else { teardown(); return }
                    for name in [AVCaptureSession.runtimeErrorNotification, AVCaptureSession.didStopRunningNotification] {
                        observers.append(NotificationCenter.default.addObserver(forName: name, object: session, queue: nil) { [weak self] _ in
                            self?.queue.async { [weak self] in self?.finish(.unsupported("recipient.camera_interrupted")) }
                        })
                    }
                    session.startRunning()
                    if isCancelled { teardown() }
                    else if !session.isRunning { finish(.unsupported("recipient.camera")) }
                } catch { finish(.unsupported("recipient.camera")) }
            }
        }
        func stop() {
            cancellation.withLock { cancelled = true }
            queue.async { [self] in ended = true; teardown() }
        }
        private func configure() throws {
            guard AVCaptureDevice.authorizationStatus(for: .video) == .authorized,
                  let camera = AVCaptureDevice.default(for: .video) else { throw FTError.unsupported("recipient.camera") }
            let input = try AVCaptureDeviceInput(device: camera)
            session.beginConfiguration()
            defer { session.commitConfiguration() }
            if session.canSetSessionPreset(.hd1280x720) { session.sessionPreset = .hd1280x720 }
            else if session.canSetSessionPreset(.vga640x480) { session.sessionPreset = .vga640x480 }
            else { throw FTError.unsupported("recipient.camera") }
            guard session.canAddInput(input) else { throw FTError.unsupported("recipient.camera") }
            session.addInput(input)
            let output = AVCaptureMetadataOutput()
            guard session.canAddOutput(output) else { throw FTError.unsupported("recipient.camera") }
            session.addOutput(output); metadata = output
            guard output.availableMetadataObjectTypes.contains(.qr) else { throw FTError.unsupported("recipient.camera") }
            output.setMetadataObjectsDelegate(self, queue: queue); output.metadataObjectTypes = [.qr]
        }
        private func teardown() {
            for observer in observers { NotificationCenter.default.removeObserver(observer) }
            observers.removeAll()
            metadata?.setMetadataObjectsDelegate(nil, queue: nil); metadata = nil
            if session.isRunning { session.stopRunning() }
            session.beginConfiguration()
            for output in session.outputs { session.removeOutput(output) }
            for input in session.inputs { session.removeInput(input) }
            session.commitConfiguration()
        }
        private func finish(_ error: FTError) {
            guard !ended else { return }
            ended = true
            // Return the metadata callback before stopping on its owning queue.
            queue.async { [self] in
                teardown()
                guard !isCancelled else { return }
                Task { @MainActor [onError] in onError(error) }
            }
        }
        func metadataOutput(_ output: AVCaptureMetadataOutput, didOutput metadataObjects: [AVMetadataObject], from connection: AVCaptureConnection) {
            guard !ended, !isCancelled else { return }
            guard metadataObjects.count <= 8 else { finish(.malformed("recipient.multiple_qr")); return }
            let candidates = metadataObjects.compactMap { object -> String? in
                guard let code = object as? AVMetadataMachineReadableCodeObject, code.type == .qr,
                      let text = code.stringValue, text.hasPrefix("FTREQ1:") else { return nil }
                return text
            }
            guard !candidates.isEmpty else { return }
            guard candidates.count == 1 else { finish(.malformed("recipient.multiple_qr")); return }
            let text = candidates[0]
            guard text.utf8.count <= FTLimit.text else { finish(.limit("recipient.request")); return }
            ended = true
            queue.async { [self] in
                teardown()
                guard !isCancelled else { return }
                Task { @MainActor [onRequest] in onRequest(text) }
            }
        }
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
            Text(verbatim: request.algorithm + "\n" + request.fingerprint)
                .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
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
