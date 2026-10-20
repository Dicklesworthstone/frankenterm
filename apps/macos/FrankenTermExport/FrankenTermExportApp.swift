import SwiftUI
import Observation
import AppKit
import UniformTypeIdentifiers
import ImageIO
import FrankenTermKit

private typealias SourceKeyUnlockRequest = OpenSSHSourceKeyUnlocker<AppleCryptography>.Request

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
/// Only a selected workspace and explicitly chosen key files are ever opened.
private actor MacExportEngine {
    struct Prepared: Sendable {
        let comparison: String, pages: Int, bytes: Int
    }
    struct SelectionState: Sendable {
        let source: WorkspaceDocument
        let selection: WorkspaceExportSelection
        let decisions: [String: WorkspaceExportSelection.IdentityDecision]
        let loadedFingerprints: Set<String>
        let review: WorkspaceExportSelection.Review
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
    private var decisions: [String: WorkspaceExportSelection.IdentityDecision] = [:]
    private var keys: [PrivateKeyRecord] = []
    private var exported: FTAG.Export?
    private var sender: OpticalSender?

    private func fingerprint(_ key: PrivateKeyRecord) -> String {
        "SHA256:" + crypto.sha256(key.publicBlob).base64EncodedString().replacingOccurrences(of: "=", with: "")
    }
    private func state() throws -> SelectionState {
        guard let document, let selection else { throw FTError.stale }
        let loaded = Set(keys.map(fingerprint))
        return .init(source: document, selection: selection, decisions: decisions, loadedFingerprints: loaded,
                     review: try selection.review(decisions: decisions, loadedFingerprints: loaded))
    }
    private func invalidateExport() { exported = nil; sender = nil }
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
        return try state()
    }
    func selectGroups(_ ids: Set<String>) throws -> SelectionState {
        retireKeyRequest(); invalidateExport()
        guard let document else { throw FTError.stale }
        let selected = try WorkspaceExportSelection(workspace: document, groupIDs: ids)
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
        // Cancellation after the KDF returns but before that handoff rolls back
        // only this operation's installation, including a replaced identity.
        pendingKeyInstallation = .init(requestID: requestID, previousKeys: keys, previousDecisions: decisions)
        keys = next; decisions[selectedFingerprint] = .includePrivateKey; invalidateExport()
        return try state()
    }
    func prepare() throws -> Prepared {
        retireKeyRequest(); invalidateExport()
        try Task.checkCancellation()
        guard let selection else { throw FTError.stale }
        // Each new seal is a distinct immutable export, including when a user
        // changes selected keys or prepares the same source document again.
        // Playback and file/animation saves continue reusing this one envelope.
        let archive = try selection.archive(decisions: decisions, keys: keys, bundleID: UUID().uuidString, using: crypto)
        let candidate = try FTAG.seal(archive, using: crypto)
        try Task.checkCancellation()
        let optical = try OpticalSender(envelope: candidate.envelope, using: crypto)
        exported = candidate; sender = optical
        return .init(comparison: FTAG.comparison(candidate.envelope, using: crypto), pages: optical.identity.pageCount, bytes: candidate.envelope.count)
    }
    func next(repairPage: Int?) throws -> String {
        guard var active = sender else { throw FTError.stale }
        let text = try active.next(repairPage: repairPage, using: crypto)
        sender = active
        return text
    }
    func code() throws -> String {
        guard let exported else { throw FTError.stale }
        return exported.code.reveal(using: crypto)
    }
    func encryptedEnvelope() throws -> Data {
        guard let exported else { throw FTError.stale }
        return exported.envelope
    }
    func saveEncryptedFile(to url: URL) throws {
        guard url.isFileURL, let exported else { throw FTError.stale }
        let access = url.startAccessingSecurityScopedResource()
        defer { if access { url.stopAccessingSecurityScopedResource() } }
        // Deliberate new-file save only. Never overwrite another backup silently.
        try exported.envelope.write(to: url, options: .withoutOverwriting)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
        let handle = try FileHandle(forWritingTo: url)
        try handle.synchronize(); try handle.close()
    }
    func clear() { retireKeyRequest(); document = nil; selection = nil; decisions = [:]; keys = []; invalidateExport() }
}

@MainActor
@Observable
private final class MacExportModel {
    var document: WorkspaceDocument?
    var selection: WorkspaceExportSelection?
    var identityDecisions: [String: WorkspaceExportSelection.IdentityDecision] = [:]
    var exportReview: WorkspaceExportSelection.Review?
    var includedFingerprints = Set<String>()
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
    var keyUnlockRequest: SourceKeyUnlockRequest?
    @ObservationIgnored private let engine = MacExportEngine()
    @ObservationIgnored private let renderer = NativeOpticalPresentation()
    @ObservationIgnored private let videoRenderer = NativeOpticalVideo()
    @ObservationIgnored private var playback: Task<Void, Never>?
    @ObservationIgnored private var work: Task<Void, Never>?
    @ObservationIgnored private var generation = UUID()
    @ObservationIgnored private var awake: NSObjectProtocol?
    @ObservationIgnored private var foreground = true
    @ObservationIgnored private var sourceKeyOperation: UUID?

    var selectedGroupIDs: Set<String> { Set(selection?.workspace.groups.map(\.id) ?? []) }
    private func apply(_ result: MacExportEngine.SelectionState) {
        document = result.source; selection = result.selection; identityDecisions = result.decisions
        includedFingerprints = result.loadedFingerprints; exportReview = result.review
        prepared = nil; approveSelectedKeys = false; emitted = 0; repairPage = -1
    }
    private func invalidatePrepared() {
        pause(); revealedCode = nil; prepared = nil; approveSelectedKeys = false; emitted = 0; repairPage = -1
    }
    private func retireSourceKeyPrompt() {
        sourceKeyOperation = nil; keyUnlockRequest = nil
    }

    func chooseWorkspace() {
        guard !busy else { return }
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.json, .data]; panel.message = "Choose a local .ftworkspace JSON document. Executable Mac configuration is never evaluated."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        retireSourceKeyPrompt(); invalidatePrepared()
        perform { [self] in
            apply(try await engine.loadWorkspace(url))
        }
    }
    func selectGroup(_ id: String, included: Bool) {
        guard !busy else { return }
        var groups = selectedGroupIDs
        if included { groups.insert(id) } else { groups.remove(id) }
        guard !groups.isEmpty else { return }
        retireSourceKeyPrompt(); invalidatePrepared()
        perform { [self] in apply(try await engine.selectGroups(groups)) }
    }
    func decide(_ value: WorkspaceExportSelection.IdentityDecision, fingerprint: String) {
        guard !busy else { return }
        retireSourceKeyPrompt(); invalidatePrepared()
        perform { [self] in apply(try await engine.decide(value, fingerprint: fingerprint)) }
    }
    func chooseKey(_ credentialID: String) {
        guard !busy else { return }
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.message = "Choose the matching OpenSSH Ed25519 or ECDSA key (P-256, P-384, or P-521). Protected files using bcrypt and AES-CTR are unlocked locally after you enter their passphrase."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        retireSourceKeyPrompt(); invalidatePrepared()
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
                case let .passphraseRequired(request): keyUnlockRequest = request
                }
            } catch {
                await engine.cancelKeyUnlock(operationID: token)
                if sourceKeyOperation == token { retireSourceKeyPrompt() }
                throw error
            }
        }
    }
    func unlockSourceKey(requestID: UUID, passphrase: Data) {
        guard !busy, foreground, keyUnlockRequest?.id == requestID, let token = sourceKeyOperation else { return }
        perform { [self] in
            var secret = passphrase
            defer { secret.resetBytes(in: 0..<secret.count) }
            do {
                let result = try await engine.unlockKey(operationID: token, requestID: requestID, passphrase: secret)
                try Task.checkCancellation()
                guard foreground, sourceKeyOperation == token, keyUnlockRequest?.id == requestID else { throw CancellationError() }
                retireSourceKeyPrompt(); apply(result)
                await engine.acknowledgeKey(operationID: token)
            } catch {
                let remaining = await engine.pendingKeyRequest(operationID: token)
                if !Task.isCancelled, foreground, sourceKeyOperation == token, keyUnlockRequest?.id == requestID {
                    keyUnlockRequest = remaining
                    if remaining == nil { sourceKeyOperation = nil }
                } else {
                    await engine.cancelKeyUnlock(operationID: token, requestID: requestID)
                }
                throw error
            }
        }
    }
    func cancelKeyUnlock(requestID: UUID) {
        guard keyUnlockRequest?.id == requestID, let token = sourceKeyOperation else { return }
        work?.cancel(); retireSourceKeyPrompt()
        // Bound to the dismissed request: a late sheet callback cannot discard
        // the next file selected by the user.
        Task { await engine.cancelKeyUnlock(operationID: token, requestID: requestID) }
    }
    func prepare() {
        guard !busy, approveSelectedKeys, exportReview?.canPrepare == true else { return }
        pause(); revealedCode = nil; prepared = nil; emitted = 0; repairPage = -1
        perform { [self] in prepared = try await engine.prepare(); emitted = 0; repairPage = -1 }
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
        guard !busy, prepared != nil, foreground else { return }
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
            pause(); revealedCode = nil
            if let token = sourceKeyOperation {
                work?.cancel(); retireSourceKeyPrompt()
                // A pending read or KDF checks cancellation before installing a
                // result. Retire its encrypted source after the bounded worker.
                Task { await engine.cancelKeyUnlock(operationID: token) }
            }
        }
    }
    func saveEncryptedFile() {
        guard !busy, prepared != nil else { return }
        pause(); revealedCode = nil
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-Workspace.ftairgap"
        panel.message = "Save encrypted bytes only. Keep the separately revealed transfer code in a different place. Choose a new filename."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        perform { [self] in try await engine.saveEncryptedFile(to: url); message = "Encrypted workspace saved. The transfer code is not included in this file." }
    }
    func saveAnimation() {
        guard !busy, prepared != nil else { return }
        pause(); revealedCode = nil
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-QR.apng"
        panel.allowedContentTypes = [.png]; panel.allowsOtherFileTypes = true
        panel.message = "Save a lossless animated PNG. Every encoded frame is checked before the final file is published. Choose a new filename."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let speed = rate
        perform { [self] in
            let bytes = try await engine.encryptedEnvelope()
            let count = try await renderer.saveAnimation(envelope: bytes, to: url, symbolsPerSecond: speed)
            message = "Saved and verified \(count) encrypted QR symbols. Play the APNG without interpolation; keep its code separate."
        }
    }
    func saveVideo() {
        guard !busy, prepared != nil else { return }
        pause(); revealedCode = nil
        let panel = NSSavePanel(); panel.nameFieldStringValue = "FrankenTerm-QR.mp4"
        panel.allowedContentTypes = [.mpeg4Movie]
        panel.message = "Save an encrypted QR video. Every decoded frame and the reconstructed package must pass verification before the new file is published."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        let speed = rate
        perform { [self] in
            let source = try OpticalVideoSource(envelope: await engine.encryptedEnvelope())
            let receipt = try await videoRenderer.save(source: source, to: url, symbolsPerSecond: speed)
            let duration = String(format: "%.1f", receipt.durationSeconds)
            message = "Saved and verified \(receipt.symbolCount) encrypted QR symbols in a \(duration)-second MP4 (\(receipt.frameCount) frames). Keep its transfer code separate."
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
        retireSourceKeyPrompt(); invalidatePrepared(); document = nil; selection = nil; identityDecisions = [:]; exportReview = nil
        includedFingerprints = []; prepared = nil; approveSelectedKeys = false; repairPage = -1
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
                    Button("Open Workspace…") { model.chooseWorkspace() }.disabled(model.busy)
                }
                Text("Offline workspace transfer").font(.title2).foregroundStyle(.secondary)
                if let document = model.document {
                    exportChoices(document)
                } else {
                    ContentUnavailableView("Choose a workspace", systemImage: "rectangle.3.group",
                        description: Text("Open a local .ftworkspace document, then explicitly select its matching private keys. Nothing is read from your SSH folder automatically."))
                }
                if let prepared = model.prepared {
                    HStack {
                        Text(verbatim: prepared.comparison).font(.system(.title3, design: .monospaced).bold())
                        Spacer()
                        Text("\(prepared.pages) pages · \(prepared.bytes) encrypted bytes").font(.caption)
                    }
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
                        Button("Reveal Code Separately") { model.revealCode() }.disabled(model.busy)
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
                    Text("MP4 video and lossless APNG animation reuse this package’s encrypted bytes. Each saved recording has a finite repair set; live playback can generate more. The transfer code is never included in exported media.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if let message = model.message { Text(verbatim: message).font(.callout).textSelection(.enabled) }
            }.padding(28)
        }.frame(minWidth: 760, minHeight: 680)
            .sheet(item: Binding(get: { model.keyUnlockRequest }, set: { value in
                if value == nil, let request = model.keyUnlockRequest { model.cancelKeyUnlock(requestID: request.id) }
            })) { request in
                SourceKeyUnlockSheet(model: model, request: request).id(request.id)
            }
            .onChange(of: scenePhase) { _, phase in model.setForeground(phase == .active) }
            .onDisappear { model.setForeground(false) }
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
                                  : finding.setting == "credential_policy" ? "Credential policy preserved" : "Preserved with an omission",
                                  systemImage: finding.disposition == 1 ? "lock.trianglebadge.exclamationmark" : "info.circle")
                                .font(.caption.bold())
                            Text(verbatim: finding.entityID + ": " + finding.reason).font(.caption)
                        }
                    }
                    Text("The phone evaluates its existing identities, reviewed host trust, and transport support separately. Required policies remain enforced after restoring a narrower workspace.")
                        .font(.caption).foregroundStyle(.secondary)
                    Toggle("I approve these selected groups, \(review.includedIdentityCount) private identities, and the omissions shown above.", isOn: $model.approveSelectedKeys)
                        .disabled(model.busy || !review.canPrepare)
                    HStack {
                        Button("Prepare Encrypted Transfer") { model.prepare() }.buttonStyle(.borderedProminent)
                            .disabled(model.busy || !model.approveSelectedKeys || !review.canPrepare)
                        Button("Clear Loaded Material", role: .destructive) { model.clear() }.disabled(model.busy)
                        if model.busy { ProgressView().controlSize(.small) }
                    }
                }.frame(maxWidth: .infinity, alignment: .leading).padding(10)
            }
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

@MainActor
private struct SourceKeyUnlockSheet: View {
    @Bindable var model: MacExportModel
    let request: SourceKeyUnlockRequest
    @State private var passphrase = ""
    @Environment(\.scenePhase) private var scenePhase

    private var canUnlock: Bool {
        !model.busy && model.keyUnlockRequest?.id == request.id &&
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
            .onChange(of: scenePhase) { _, phase in
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
