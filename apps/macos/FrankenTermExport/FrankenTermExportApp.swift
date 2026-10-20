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
    private let crypto = AppleCryptography()
    private var document: WorkspaceDocument?
    private var keys: [PrivateKeyRecord] = []
    private var exported: FTAG.Export?
    private var sender: OpticalSender?

    func loadWorkspace(_ url: URL) throws -> WorkspaceDocument {
        let candidate = try WorkspaceDocument(data: SelectedLocalFile.read(url, limit: FTLimit.manifest))
        document = candidate; keys = []; exported = nil; sender = nil
        return candidate
    }
    func loadKey(_ url: URL, credentialID: String) throws -> Set<String> {
        guard let document, let reference = document.credentials.first(where: { $0.id == credentialID }) else { throw FTError.stale }
        let key = try PrivateKeyRecord.fromPEM(SelectedLocalFile.read(url, limit: FTLimit.key), credentialID: credentialID, using: crypto)
        let fingerprint = "SHA256:" + crypto.sha256(key.publicBlob).base64EncodedString().replacingOccurrences(of: "=", with: "")
        guard reference.fingerprint == fingerprint else { throw FTError.malformed("export.key_does_not_match") }
        var next = keys.filter { $0.credentialID != credentialID }
        // A single material record resolves all aliases of the same public identity.
        if !next.contains(where: { $0.publicBlob == key.publicBlob }) { next.append(key) }
        _ = try AirGapArchive(workspace: document, keys: next, using: crypto)
        keys = next; exported = nil; sender = nil
        return Set(next.map { "SHA256:" + crypto.sha256($0.publicBlob).base64EncodedString().replacingOccurrences(of: "=", with: "") })
    }
    func prepare() throws -> Prepared {
        try Task.checkCancellation()
        guard let document else { throw FTError.stale }
        let archive = try AirGapArchive(workspace: document, keys: keys, using: crypto)
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
    func clear() { document = nil; keys = []; exported = nil; sender = nil }
}

@MainActor
@Observable
private final class MacExportModel {
    var document: WorkspaceDocument?
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
    @ObservationIgnored private var playback: Task<Void, Never>?
    @ObservationIgnored private var work: Task<Void, Never>?
    @ObservationIgnored private var generation = UUID()
    @ObservationIgnored private var awake: NSObjectProtocol?
    @ObservationIgnored private var foreground = true

    func chooseWorkspace() {
        guard !busy else { return }
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.json, .data]; panel.message = "Choose a local .ftworkspace JSON document. Executable Mac configuration is never evaluated."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        pause(); revealedCode = nil
        perform { [self] in
            let result = try await engine.loadWorkspace(url)
            document = result; includedFingerprints = []; prepared = nil; approveSelectedKeys = false; emitted = 0
        }
    }
    func chooseKey(_ credentialID: String) {
        guard !busy else { return }
        let panel = NSOpenPanel(); panel.canChooseDirectories = false; panel.allowsMultipleSelection = false
        panel.message = "Choose the matching unencrypted OpenSSH Ed25519 key. Other formats are explicitly unsupported in this build."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        pause(); revealedCode = nil
        perform { [self] in
            includedFingerprints = try await engine.loadKey(url, credentialID: credentialID)
            prepared = nil; approveSelectedKeys = false
        }
    }
    func prepare() {
        guard !busy, approveSelectedKeys else { return }
        pause(); revealedCode = nil
        perform { [self] in prepared = try await engine.prepare(); emitted = 0 }
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
    func cancelWork() { work?.cancel() }
    func clear() {
        guard !busy else { return }
        pause(); revealedCode = nil; document = nil; includedFingerprints = []; prepared = nil; approveSelectedKeys = false
        perform { [self] in await engine.clear() }
    }
    private func report(_ error: Error) {
        if let error = error as? FTError {
            if error == .limit("animation.use_live_transfer") {
                message = "This package exceeds the 256-symbol saved-animation limit. Use live QR playback or save the encrypted .ftairgap file. No settings or keys were omitted."
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
                    GroupBox {
                        VStack(alignment: .leading, spacing: 12) {
                            Text(verbatim: document.name).font(.headline)
                            Text("\(document.machines.count) machines · \(document.credentials.count) credential references")
                            ForEach(document.credentials) { credential in
                                HStack {
                                    VStack(alignment: .leading) {
                                        Text(verbatim: credential.id).font(.headline)
                                        Text(verbatim: credential.fingerprint).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                                    }
                                    Spacer()
                                    if model.includedFingerprints.contains(credential.fingerprint) { Label("Included", systemImage: "checkmark.shield") }
                                    Button("Choose Matching Key…") { model.chooseKey(credential.id) }.disabled(model.busy)
                                }
                            }
                            Text("This build supports unencrypted OpenSSH Ed25519 files. Encrypted key files, RSA/ECDSA, certificates, hardware keys, and automatic capture of the running Mac workspace are not implemented yet.")
                                .font(.caption).foregroundStyle(.secondary)
                            Toggle("I approve exporting these settings and \(model.includedFingerprints.count) selected private identities.", isOn: $model.approveSelectedKeys)
                            HStack {
                                Button("Prepare Encrypted Transfer") { model.prepare() }.buttonStyle(.borderedProminent)
                                    .disabled(model.busy || !model.approveSelectedKeys)
                                Button("Clear Loaded Material", role: .destructive) { model.clear() }.disabled(model.busy)
                            }
                        }.frame(maxWidth: .infinity, alignment: .leading).padding(10)
                    }
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
                        Button("Save Verified QR Animation…") { model.saveAnimation() }.disabled(model.busy)
                        if model.busy { ProgressView().controlSize(.small); Button("Cancel Work") { model.cancelWork() } }
                    }
                    Text("Saved animation uses lossless APNG, not MP4. A recording repeats a finite repair set; live playback can generate more. The code is never included in exported media.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if let message = model.message { Text(verbatim: message).font(.callout).textSelection(.enabled) }
            }.padding(28)
        }.frame(minWidth: 760, minHeight: 680)
            .onChange(of: scenePhase) { _, phase in model.setForeground(phase == .active) }
            .onDisappear { model.setForeground(false) }
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
