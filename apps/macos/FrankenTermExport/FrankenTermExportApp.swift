import SwiftUI
import Observation
import AppKit
import UniformTypeIdentifiers
import ImageIO
import FrankenTermKit

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
    private let crypto = AppleCryptography()
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
    func loadWorkspace(_ url: URL) throws -> SelectionState {
        let candidate = try WorkspaceDocument(data: SelectedLocalFile.read(url, limit: FTLimit.manifest))
        let selected = try WorkspaceExportSelection(workspace: candidate, groupIDs: [candidate.initialGroupID])
        document = candidate; selection = selected; decisions = [:]; keys = []; invalidateExport()
        return try state()
    }
    func selectGroups(_ ids: Set<String>) throws -> SelectionState {
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
        guard let selection, selection.identities.contains(where: { $0.fingerprint == selectedFingerprint }) else { throw FTError.stale }
        decisions[selectedFingerprint] = value
        if value != .includePrivateKey { keys.removeAll { fingerprint($0) == selectedFingerprint } }
        invalidateExport()
        return try state()
    }
    func loadKey(_ url: URL, credentialID: String) throws -> SelectionState {
        guard let document, let selection,
              let reference = selection.workspace.credentials.first(where: { $0.id == credentialID }) else { throw FTError.stale }
        let key = try PrivateKeyRecord.fromPEM(SelectedLocalFile.read(url, limit: FTLimit.key), credentialID: credentialID, using: crypto)
        let selectedFingerprint = fingerprint(key)
        guard reference.fingerprint == selectedFingerprint else { throw FTError.malformed("export.key_does_not_match") }
        var next = keys.filter { fingerprint($0) != selectedFingerprint }
        // A single material record resolves all aliases of the same public identity.
        next.append(key)
        _ = try AirGapArchive(workspace: document, keys: next, using: crypto)
        keys = next; decisions[selectedFingerprint] = .includePrivateKey; invalidateExport()
        return try state()
    }
    func prepare() throws -> Prepared {
        invalidateExport()
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
    func clear() { document = nil; selection = nil; decisions = [:]; keys = []; invalidateExport() }
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
    @ObservationIgnored private let engine = MacExportEngine()
    @ObservationIgnored private let renderer = NativeOpticalPresentation()
    @ObservationIgnored private let videoRenderer = NativeOpticalVideo()
    @ObservationIgnored private var playback: Task<Void, Never>?
    @ObservationIgnored private var work: Task<Void, Never>?
    @ObservationIgnored private var generation = UUID()
    @ObservationIgnored private var awake: NSObjectProtocol?
    @ObservationIgnored private var foreground = true

    var selectedGroupIDs: Set<String> { Set(selection?.workspace.groups.map(\.id) ?? []) }
    private func apply(_ result: MacExportEngine.SelectionState) {
        document = result.source; selection = result.selection; identityDecisions = result.decisions
        includedFingerprints = result.loadedFingerprints; exportReview = result.review
        prepared = nil; approveSelectedKeys = false; emitted = 0; repairPage = -1
    }
    private func invalidatePrepared() {
        pause(); revealedCode = nil; prepared = nil; approveSelectedKeys = false; emitted = 0; repairPage = -1
    }

    func chooseWorkspace() {
        guard !busy else { return }
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.json, .data]; panel.message = "Choose a local .ftworkspace JSON document. Executable Mac configuration is never evaluated."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        invalidatePrepared()
        perform { [self] in
            apply(try await engine.loadWorkspace(url))
        }
    }
    func selectGroup(_ id: String, included: Bool) {
        guard !busy else { return }
        var groups = selectedGroupIDs
        if included { groups.insert(id) } else { groups.remove(id) }
        guard !groups.isEmpty else { return }
        invalidatePrepared()
        perform { [self] in apply(try await engine.selectGroups(groups)) }
    }
    func decide(_ value: WorkspaceExportSelection.IdentityDecision, fingerprint: String) {
        guard !busy else { return }
        invalidatePrepared()
        perform { [self] in apply(try await engine.decide(value, fingerprint: fingerprint)) }
    }
    func chooseKey(_ credentialID: String) {
        guard !busy else { return }
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.message = "Choose the matching unencrypted OpenSSH Ed25519 or ECDSA key (P-256, P-384, or P-521). Encrypted source files and other key families are not supported yet."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        invalidatePrepared()
        perform { [self] in
            apply(try await engine.loadKey(url, credentialID: credentialID))
        }
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
        if !value { pause(); revealedCode = nil }
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
    func cancelWork() { work?.cancel() }
    func clear() {
        guard !busy else { return }
        invalidatePrepared(); document = nil; selection = nil; identityDecisions = [:]; exportReview = nil
        includedFingerprints = []; prepared = nil; approveSelectedKeys = false; repairPage = -1
        perform { [self] in await engine.clear() }
    }
    private func report(_ error: Error) {
        if let error = error as? FTError {
            if error == .limit("animation.use_live_transfer") {
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
                    Text("Supported source containers: unencrypted OpenSSH Ed25519 and ECDSA P-256, P-384, or P-521. Encrypted key files, RSA, certificates, hardware keys, and capture from the running Mac workspace need additional support.")
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
