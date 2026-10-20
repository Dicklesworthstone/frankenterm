import SwiftUI
import Observation
import VisionKit
import AVFoundation
import UniformTypeIdentifiers
import FrankenTermKit

@main
@MainActor
struct FrankenTermMobileApp: App {
    @State private var model = MobileWorkspaceModel()
    var body: some Scene { WindowGroup { MobileWorkspaceRoot(model: model) } }
}

@MainActor
@Observable
final class MobileWorkspaceModel {
    var workspaces: [InstalledWorkspace] = []
    var availableCredentials: [UUID: Set<String>] = [:]
    var state: OfflineRestoreStatus?
    var message: String?
    var showRestore = false
    var cameraRequested = false
    var active = false
    var busy = false
    var requestingCameraAccess = false
    @ObservationIgnored private var store: WorkspaceStore?
    @ObservationIgnored private var restore: OfflineRestore<AppleCryptography>?
    @ObservationIgnored private var bootstrapStarted = false
    @ObservationIgnored private var epoch = UUID()
    @ObservationIgnored private var lifecycle: Task<Void, Never>?
    @ObservationIgnored private var observationTask: Task<Void, Never>?
    @ObservationIgnored private var observations: AsyncStream<CameraObservation>.Continuation?
    @ObservationIgnored private var lastObservation: String?
    @ObservationIgnored private var lastObservationTime = ContinuousClock.now
    private struct CameraObservation: Sendable { let text: String, generation: UUID, epoch: UUID }

    func requestCamera() {
        guard active, !busy, !requestingCameraAccess else { return }
        if !showRestore { setRestoreVisible(true) }
        guard DataScannerViewController.isSupported else {
            message = "This device does not support the native scanner. Open a local encrypted file instead."
            return
        }
        let status = AVCaptureDevice.authorizationStatus(for: .video)
        if status == .authorized { cameraRequested = true; return }
        guard status == .notDetermined else {
            message = "Camera access is denied or restricted. Allow it in Settings or use a local encrypted file."
            return
        }
        requestingCameraAccess = true
        Task { [weak self] in
            let granted = await AVCaptureDevice.requestAccess(for: .video)
            guard let self else { return }
            self.requestingCameraAccess = false
            guard self.showRestore else { return }
            // The permission sheet can deactivate the scene. Do not restart a
            // camera in an inactive scene or after the restore sheet was closed.
            if granted, self.active { self.cameraRequested = true }
            else if granted { self.message = "Camera access is ready. Tap Open Camera or Resume Camera when you return." }
            else { self.message = "Camera access was not granted. The local encrypted-file import is still available." }
        }
    }

    func bootstrap() async {
        guard !bootstrapStarted else { return }
        bootstrapStarted = true
        do {
            let created = try await Task.detached(priority: .userInitiated) {
                guard let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first else { throw FTError.storage }
                return WorkspaceStore(persistence: try FileWorkspacePersistence(directory: support.appendingPathComponent("FrankenTermWorkspaces", isDirectory: true)),
                                      vault: AppleWorkspaceVault())
            }.value
            store = created; restore = OfflineRestore(using: AppleCryptography(), store: created)
            try await refreshWorkspaces(using: created, epoch: epoch)
            updateLifecycle()
        } catch { report(error); bootstrapStarted = false }
    }
    func setActive(_ value: Bool) {
        active = value
        if !value { cameraRequested = false; state = nil; availableCredentials = [:] }
        updateLifecycle()
    }
    func setRestoreVisible(_ value: Bool) {
        showRestore = value
        if !value { cameraRequested = false }
        updateLifecycle()
    }
    private func updateLifecycle() {
        epoch = UUID()
        observations?.finish(); observations = nil
        observationTask?.cancel(); observationTask = nil; lastObservation = nil
        guard let restore else { return }
        let ticket = epoch, visible = active && showRestore, prior = lifecycle
        lifecycle = Task { [weak self] in
            await prior?.value
            let next = visible ? await restore.resume() : await restore.suspend()
            guard let self, self.epoch == ticket else { return }
            self.state = next
            if visible { self.startObservations(restore: restore) }
            if self.active, let store = self.store {
                do { try await self.refreshWorkspaces(using: store, epoch: ticket) }
                catch { if self.active, self.epoch == ticket { self.availableCredentials = [:]; self.report(error) } }
            }
        }
    }
    private func refreshWorkspaces(using store: WorkspaceStore, epoch ticket: UUID) async throws {
        let snapshot = try await store.loadWithCredentialAvailability()
        guard active, epoch == ticket else { return }
        workspaces = snapshot.map(\.workspace)
        availableCredentials = Dictionary(uniqueKeysWithValues: snapshot.map { ($0.workspace.id, $0.credentialIDs) })
    }
    private func startObservations(restore: OfflineRestore<AppleCryptography>) {
        let channel = AsyncStream<CameraObservation>.makeStream(bufferingPolicy: .bufferingNewest(8))
        observations = channel.continuation
        observationTask = Task { [weak self] in
            for await observation in channel.stream {
                guard !Task.isCancelled, let self else { break }
                guard self.active, self.showRestore, self.epoch == observation.epoch else { continue }
                do {
                    let next = try await restore.observe(observation.text, generation: observation.generation)
                    guard self.active, self.epoch == observation.epoch else { continue }
                    self.state = next
                    if ![.idle, .candidate, .collecting].contains(next.phase) { self.cameraRequested = false }
                } catch FTError.stale { continue }
                catch is CancellationError { break }
                catch { self.report(error) }
            }
        }
    }
    /// At most eight bounded strings wait on one consumer. No task-per-frame fanout.
    func observed(_ text: String) {
        guard active, showRestore, !busy, let state, !state.isSuspended,
              [.idle, .candidate, .collecting].contains(state.phase), text.hasPrefix("FTQR1/"),
              text.utf8.count <= FTLimit.qrText, text != lastObservation else { return }
        let now = ContinuousClock.now
        guard lastObservationTime.duration(to: now) >= .milliseconds(35) else { return }
        lastObservation = text; lastObservationTime = now
        observations?.yield(.init(text: text, generation: state.generation, epoch: epoch))
    }
    func run(_ action: @escaping @Sendable (OfflineRestore<AppleCryptography>, OfflineRestoreStatus) async throws -> OfflineRestoreStatus) {
        guard !busy, active, let restore else { return }
        busy = true; message = nil
        let ticket = epoch, pendingLifecycle = lifecycle
        Task { [weak self] in
            await pendingLifecycle?.value
            guard let self else { return }
            defer { self.busy = false }
            guard self.active, self.epoch == ticket else { return }
            do {
                let current = await restore.status()
                let result = try await action(restore, current)
                guard self.active, self.epoch == ticket else { return }
                self.state = result
                if result.phase == .complete, let store = self.store { try await self.refreshWorkspaces(using: store, epoch: ticket) }
            } catch {
                guard self.active, self.epoch == ticket else { return }
                self.state = await restore.status()
                self.report(error)
            }
        }
    }
    func importFile(_ url: URL) {
        guard !busy else { return }
        cameraRequested = false; setRestoreVisible(true)
        run { restore, state in
            let state = state.phase == .complete ? try await restore.reset() : state
            let bytes = try await Task.detached(priority: .userInitiated) { try SelectedLocalFile.read(url, limit: FTLimit.envelope) }.value
            return try await restore.acceptEncryptedFile(bytes, generation: state.generation)
        }
    }
    func report(_ error: Error) {
        if let error = error as? FTError { message = error.localizedDescription }
        else if let error = error as? WorkspaceStoreError { message = error.localizedDescription }
        else if !(error is CancellationError) { message = "The operation could not finish. Your existing workspace was not replaced without a completed import. Check saved workspaces before retrying." }
    }
}

@MainActor
struct MobileWorkspaceRoot: View {
    @Bindable var model: MobileWorkspaceModel
    @Environment(\.scenePhase) private var scenePhase
    @State private var chooseFile = false
    var body: some View {
        ZStack {
            NavigationStack {
                List {
                    Section {
                        Label("Your machines. Your colors.", systemImage: "terminal.fill").font(.title2.bold()).padding(.vertical, 8)
                        Text("Restore your encrypted Mac workspace without a network connection. Private keys go into this device’s protected Keychain.")
                            .foregroundStyle(.secondary)
                        Button { model.requestCamera() } label: {
                            Label("Restore from Animated QR", systemImage: "qrcode.viewfinder")
                        }.disabled(model.state == nil)
                        Button { chooseFile = true } label: { Label("Open Local Encrypted File", systemImage: "doc.badge.arrow.up") }
                            .disabled(model.state == nil || model.busy)
                    }
                    Section("Saved workspaces") {
                        if model.workspaces.isEmpty { Text("No workspace restored yet.").foregroundStyle(.secondary) }
                        ForEach(model.workspaces) { saved in
                            if let document = try? saved.document() {
                                NavigationLink { MachineWorkspaceView(document: document, workspaceID: saved.id, model: model) } label: {
                                    VStack(alignment: .leading, spacing: 5) {
                                        Text(verbatim: document.name).font(.headline).lineLimit(1)
                                        if let available = model.availableCredentials[saved.id] {
                                            Text("\(document.machines.count) machines · \(available.count) available key references · Connect later")
                                                .font(.caption).foregroundStyle(.secondary)
                                        } else {
                                            Text("\(document.machines.count) machines · Key availability needs protected storage")
                                                .font(.caption).foregroundStyle(.secondary)
                                        }
                                    }.padding(.vertical, 4)
                                }
                            }
                        }
                    }
                    Section {
                        Text("Development build: offline transfer and workspace restoration. Live SSH sessions and the Rust/Metal terminal are not integrated yet.")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                }.navigationTitle("FrankenTerm")
            }
            if !model.active { Color(.systemBackground).ignoresSafeArea().overlay { Label("FrankenTerm", systemImage: "lock.shield").font(.title2) } }
        }
        .task { model.setActive(scenePhase == .active); await model.bootstrap() }
        .onChange(of: scenePhase) { _, phase in model.setActive(phase == .active) }
        .sheet(isPresented: Binding(get: { model.showRestore }, set: { model.setRestoreVisible($0) })) {
            OfflineRestoreSheet(model: model)
        }
        .fileImporter(isPresented: $chooseFile, allowedContentTypes: [.data], allowsMultipleSelection: false) { result in
            switch result {
            case .success(let urls): if let url = urls.first { model.importFile(url) }
            case .failure(let error): model.report(error)
            }
        }
        .alert("FrankenTerm", isPresented: Binding(get: { !model.showRestore && model.message != nil }, set: { if !$0 { model.message = nil } })) {
            Button("OK") { model.message = nil }
        } message: { Text(verbatim: model.message ?? "") }
    }
}

@MainActor
struct OfflineRestoreSheet: View {
    @Bindable var model: MobileWorkspaceModel
    @State private var code = ""
    @State private var confirmed = false
    @State private var allowRemoval = false
    @State private var allowRollback = false
    var body: some View {
        NavigationStack {
            Group {
                if !model.active { ContentUnavailableView("Locked", systemImage: "lock.shield", description: Text("Return to FrankenTerm to continue.")) }
                else if let state = model.state {
                    ScrollView {
                        VStack(alignment: .leading, spacing: 20) {
                            Text(title(state.phase)).font(.title2.bold())
                            if let message = model.message {
                                VStack(alignment: .leading, spacing: 8) {
                                    Label { Text(verbatim: message) } icon: { Image(systemName: "exclamationmark.circle") }
                                    Button("Dismiss Message") { model.message = nil }
                                }.font(.callout).padding().background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
                            }
                            if let comparison = state.comparison {
                                VStack(alignment: .leading, spacing: 6) {
                                    Text("Compare with your Mac").font(.caption).foregroundStyle(.secondary)
                                    Text(verbatim: comparison).font(.system(.headline, design: .monospaced)).minimumScaleFactor(0.6).lineLimit(1)
                                }.padding().frame(maxWidth: .infinity, alignment: .leading).background(.quaternary, in: RoundedRectangle(cornerRadius: 16))
                            }
                            if [.idle, .collecting].contains(state.phase), model.cameraRequested, !state.isSuspended {
                                if DataScannerViewController.isSupported && DataScannerViewController.isAvailable {
                                    OpticalCamera(onCode: model.observed, onError: model.report)
                                        .frame(height: 290).clipShape(RoundedRectangle(cornerRadius: 20))
                                } else {
                                    Text("Camera scanning is unavailable. Allow Camera access in Settings or open an already-local .ftairgap file.")
                                }
                            }
                            switch state.phase {
                            case .idle:
                                Text("Point at the Mac’s Offline QR Transfer. No network service is used.")
                                Button("Open Camera") { model.requestCamera() }.disabled(model.requestingCameraAccess)
                            case .candidate:
                                Text("A transfer is in view. Confirm that this is the Mac you intend to use before collecting it.")
                                Button("Use This Transfer") {
                                    guard let digest = state.identity?.envelopeDigest else { return }
                                    model.run { restore, state in try await restore.selectCandidate(generation: state.generation, digest: digest) }
                                }.buttonStyle(.borderedProminent)
                            case .collecting:
                                ProgressView(value: Double(state.verifiedBytes), total: Double(state.identity?.envelopeLength ?? 1))
                                Text("\(state.verifiedPages) of \(state.identity?.pageCount ?? 0) pages verified · \(state.verifiedBytes) bytes")
                                Text("\(state.independentParts) independent fragments in unfinished pages. Duplicate frames do not count as progress.")
                                    .font(.caption).foregroundStyle(.secondary)
                                Button(model.cameraRequested ? "Pause Camera" : "Resume Camera") {
                                    if model.cameraRequested { model.cameraRequested = false } else { model.requestCamera() }
                                }.disabled(model.requestingCameraAccess)
                            case .locked:
                                Text("The complete encrypted package is collected. Reveal the transfer code separately on your Mac and enter it here.")
                                SecureField("Transfer code", text: $code).textInputAutocapitalization(.characters).autocorrectionDisabled()
                                    .textFieldStyle(.roundedBorder).onChange(of: code) { _, value in if value.utf8.count > 128 { code = String(value.prefix(128)) } }
                                Button("Decrypt and Review") {
                                    let entered = code; code = ""; confirmed = false; allowRemoval = false; allowRollback = false
                                    model.run { restore, state in try await restore.unlock(enteredCode: entered, generation: state.generation) }
                                }.buttonStyle(.borderedProminent).disabled(code.isEmpty)
                            case .preparingReview, .committing:
                                ProgressView(state.phase == .committing ? "Saving protected workspace…" : "Preparing review…")
                            case .review:
                                if let review = state.review {
                                    Text(verbatim: review.document.name).font(.headline)
                                    Text("\(review.document.machines.count) machines · \(review.includedFingerprints.count) private identities included")
                                    ForEach(review.document.machines) { machine in
                                        let theme = review.document.themes.first { $0.id == machine.themeID }
                                        HStack {
                                            Circle().fill(Color(ftHex: theme?.accent ?? "#777777")).frame(width: 12, height: 12)
                                            Text(verbatim: machine.name).lineLimit(1)
                                        }
                                    }
                                    ForEach(Array(review.includedFingerprints.enumerated()), id: \.offset) { _, fingerprint in
                                        Text(verbatim: fingerprint).font(.system(.caption2, design: .monospaced)).textSelection(.enabled)
                                    }
                                    DisclosureGroup("Host identities and connection policy") {
                                        VStack(alignment: .leading, spacing: 16) {
                                            ForEach(review.document.hostTrust) { trust in
                                                VStack(alignment: .leading, spacing: 5) {
                                                    Text(verbatim: "\(trust.host):\(trust.port)").font(.headline)
                                                    if let alias = trust.hostKeyAlias { Text(verbatim: "Host-key alias: \(alias)").font(.caption) }
                                                    ForEach(Array(trust.pins.enumerated()), id: \.offset) { _, pin in
                                                        Text(verbatim: "\(pin.algorithm) · \(pin.fingerprint)")
                                                            .font(.system(.caption2, design: .monospaced)).textSelection(.enabled)
                                                    }
                                                    if !trust.revokedKeys.isEmpty {
                                                        Text("\(trust.revokedKeys.count) explicitly revoked host identities remain blocked.").font(.caption)
                                                    }
                                                }
                                            }
                                            ForEach(review.document.routes) { route in
                                                VStack(alignment: .leading, spacing: 5) {
                                                    Text(verbatim: "\(route.user)@\(route.host):\(route.port)").font(.headline)
                                                    Text("\(route.credentialIDs.count) permitted key references · \(route.jumpRoutes.count) jump routes")
                                                        .font(.caption)
                                                }
                                            }
                                        }.padding(.top, 8)
                                    }
                                    if review.inactiveSettingCount > 0 { Text("\(review.inactiveSettingCount) desktop-only settings are preserved but inactive.") }
                                    ForEach(Array(review.findings.enumerated()), id: \.offset) { _, finding in
                                        VStack(alignment: .leading) {
                                            Text(verbatim: finding.setting).font(.caption.bold())
                                            Text(verbatim: finding.reason).font(.caption)
                                        }
                                    }
                                    Toggle("I verified the comparison value on my Mac and approve these settings, trust records, and keys.", isOn: $confirmed)
                                    if review.removesMachines { Toggle("Replace this workspace even though some existing machines are removed.", isOn: $allowRemoval) }
                                    if review.requiresRollbackApproval {
                                        VStack(alignment: .leading, spacing: 8) {
                                            Label("Previously restored recording", systemImage: "clock.arrow.circlepath").font(.headline)
                                            Text("This package was accepted before and differs from the workspace currently saved on this phone. Restoring it replaces the current settings. Local revocations stay in effect.")
                                                .font(.callout)
                                            Toggle("I approve restoring this previously accepted workspace version.", isOn: $allowRollback)
                                        }.padding().background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
                                    }
                                    Text("No connection or remote command will run. A device passcode is required for private-key storage.").font(.footnote).foregroundStyle(.secondary)
                                    Button("Restore Offline") {
                                        let token = review.token, consent = confirmed, removal = allowRemoval, rollback = allowRollback
                                        model.run { restore, state in
                                            try await restore.commit(reviewToken: token, generation: state.generation,
                                                                     sourceAndContentsConfirmed: consent, allowMachineRemoval: removal,
                                                                     allowHistoricalRollback: rollback)
                                        }
                                    }.buttonStyle(.borderedProminent)
                                        .disabled(!confirmed || (review.removesMachines && !allowRemoval) || (review.requiresRollbackApproval && !allowRollback))
                                }
                            case .complete:
                                Label("Restored offline — Connect later", systemImage: "checkmark.shield.fill").font(.headline)
                                if let receipt = state.receipt {
                                    Text("\(receipt.missingCredentialIDs.count) credential references still need a key.")
                                    if receipt.cleanupPending { Text("The workspace is saved. Cleanup will be reconciled when storage is available.") }
                                }
                                Button("Done") { model.setRestoreVisible(false) }.buttonStyle(.borderedProminent)
                            case .recoveryRequired:
                                Text("The save may have completed. Check the saved result before attempting another import.")
                                Button("Reconcile Saved Workspace") {
                                    model.run { restore, state in try await restore.reconcile(generation: state.generation) }
                                }.buttonStyle(.borderedProminent)
                            }
                            if ![.committing, .preparingReview, .recoveryRequired].contains(state.phase) {
                                Button("Start a Different Transfer", role: .destructive) {
                                    code = ""; confirmed = false; allowRemoval = false; allowRollback = false; model.cameraRequested = false
                                    model.run { restore, _ in try await restore.reset() }
                                }
                            }
                            if model.busy { ProgressView() }
                        }.padding(24).disabled(model.busy)
                    }
                } else { ProgressView("Opening protected workspace…") }
            }.navigationTitle("Offline Restore").navigationBarTitleDisplayMode(.inline)
                .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Close") { model.setRestoreVisible(false) }.disabled(model.busy) } }
        }
        .interactiveDismissDisabled(model.busy)
        .onChange(of: model.state?.review?.token) { _, _ in confirmed = false; allowRemoval = false; allowRollback = false }
        .onChange(of: model.active) { _, active in if !active { code = ""; confirmed = false; allowRemoval = false; allowRollback = false } }
        .onDisappear { code = ""; confirmed = false; allowRemoval = false; allowRollback = false; model.cameraRequested = false }
    }
    private func title(_ phase: OfflineRestorePhase) -> String {
        switch phase {
        case .idle: "Scan your Mac"
        case .candidate: "Choose this source"
        case .collecting: "Collecting encrypted pages"
        case .locked: "Unlock the package"
        case .preparingReview, .review: "Review your workspace"
        case .committing: "Restoring workspace"
        case .complete: "Your workspace is restored"
        case .recoveryRequired: "Check the save result"
        }
    }
}

@MainActor
private struct OpticalCamera: UIViewControllerRepresentable {
    let onCode: (String) -> Void
    let onError: (Error) -> Void
    func makeCoordinator() -> Coordinator { Coordinator(onCode: onCode, onError: onError) }
    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(recognizedDataTypes: [.barcode(symbologies: [.qr])], qualityLevel: .balanced,
            recognizesMultipleItems: false, isHighFrameRateTrackingEnabled: false,
            isPinchToZoomEnabled: true, isGuidanceEnabled: true, isHighlightingEnabled: false)
        scanner.delegate = context.coordinator
        return scanner
    }
    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {
        guard !scanner.isScanning else { return }
        do { try scanner.startScanning() } catch { context.coordinator.failed(error) }
    }
    static func dismantleUIViewController(_ scanner: DataScannerViewController, coordinator: Coordinator) {
        scanner.stopScanning(); scanner.delegate = nil
    }
    @MainActor
    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        private let onCode: (String) -> Void
        private let onError: (Error) -> Void
        private var reportedFailure = false
        init(onCode: @escaping (String) -> Void, onError: @escaping (Error) -> Void) { self.onCode = onCode; self.onError = onError }
        func failed(_ error: Error) {
            guard !reportedFailure else { return }
            reportedFailure = true
            // Defer model mutation out of a UIViewRepresentable update callback.
            Task { @MainActor [onError] in onError(FTError.unsupported("camera.unavailable")) }
        }
        private func submit(_ items: [RecognizedItem]) {
            for item in items.prefix(4) {
                if case .barcode(let barcode) = item, let text = barcode.payloadStringValue,
                   text.hasPrefix("FTQR1/"), text.utf8.count <= FTLimit.qrText { onCode(text) }
            }
        }
        func dataScanner(_ scanner: DataScannerViewController, didAdd items: [RecognizedItem], allItems: [RecognizedItem]) { submit(items) }
        func dataScanner(_ scanner: DataScannerViewController, didUpdate items: [RecognizedItem], allItems: [RecognizedItem]) { submit(items) }
        func dataScanner(_ scanner: DataScannerViewController, didTapOn item: RecognizedItem) { submit([item]) }
        func dataScanner(_ scanner: DataScannerViewController, becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable) {
            scanner.stopScanning(); failed(error)
        }
    }
}

@MainActor
private struct MachineWorkspaceView: View {
    let document: WorkspaceDocument
    let workspaceID: UUID
    @Bindable var model: MobileWorkspaceModel
    @State private var selection = ""
    @State private var groupID = ""
    private var machines: [WorkspaceDocument.Machine] {
        let selectedGroup = groupID.isEmpty ? document.initialGroupID : groupID
        let ids = document.groups.first(where: { $0.id == selectedGroup })?.machineOrder ?? []
        let map = Dictionary(uniqueKeysWithValues: document.machines.map { ($0.id, $0) })
        return ids.compactMap { map[$0] }
    }
    var body: some View {
        TabView(selection: $selection) {
            ForEach(machines) { machine in
                let theme = document.themes.first { $0.id == machine.themeID }
                ScrollView {
                    VStack(alignment: .leading, spacing: 18) {
                        HStack { Image(systemName: "desktopcomputer"); Text(verbatim: machine.name).font(.largeTitle.bold()).lineLimit(2) }
                        HStack(spacing: 0) { ForEach(0..<16) { index in Color(ftHex: theme?.ansi[index] ?? "#000000") } }.frame(height: 24).clipShape(Capsule())
                        Label("Restored offline", systemImage: "checkmark.shield")
                        Text("Inspect the saved routes and their local requirements below. This build has no live terminal transport, and no remote session has been started.")
                            .font(.callout)
                        ForEach(machine.routeIDs, id: \.self) { id in
                            if let route = document.routes.first(where: { $0.id == id }) {
                                VStack(alignment: .leading, spacing: 10) {
                                    HStack {
                                        Label(route.mode == "frankenterm_mux" ? "Persistent mux route" : "Direct SSH route", systemImage: "point.3.connected.trianglepath.dotted")
                                            .font(.headline)
                                        if id == machine.preferredRouteID { Text("Preferred").font(.caption.bold()) }
                                    }
                                    Text(verbatim: "\(route.user)@\(route.host):\(route.port)")
                                        .font(.system(.callout, design: .monospaced)).textSelection(.enabled)
                                    if let available = model.availableCredentials[workspaceID] {
                                        Text("\(route.credentialIDs.filter(available.contains).count) of \(route.credentialIDs.count) permitted key references are available on this device.")
                                            .font(.caption)
                                        if let readiness = try? document.readiness(forRouteID: id, availableCredentialIDs: available,
                                                reviewedTrustIDs: Set(document.hostTrust.map(\.id))) {
                                            ForEach(Array(readiness.issues.enumerated()), id: \.offset) { _, issue in
                                                Label(issue.message, systemImage: "exclamationmark.circle").font(.caption)
                                            }
                                        } else {
                                            Label("Local requirements could not be validated.", systemImage: "exclamationmark.shield").font(.caption)
                                        }
                                    } else {
                                        Label("Unlock protected storage to check credential availability.", systemImage: "lock.shield").font(.caption)
                                    }
                                    if !route.jumpRoutes.isEmpty {
                                        Text("\(route.jumpRoutes.count) configured jump routes and their own dependencies must also be ready.").font(.caption)
                                    }
                                }.padding(16).frame(maxWidth: .infinity, alignment: .leading)
                                    .background(Color(ftHex: theme?.accent ?? "#3B82F6").opacity(0.14), in: RoundedRectangle(cornerRadius: 16))
                            }
                        }
                        if let saved = model.workspaces.first(where: { $0.id == workspaceID }), !saved.findings.isEmpty {
                            DisclosureGroup("Preserved settings and unresolved requirements") {
                                VStack(alignment: .leading, spacing: 14) {
                                    ForEach(Array(saved.findings.enumerated()), id: \.offset) { _, finding in
                                        VStack(alignment: .leading, spacing: 5) {
                                            Label(finding.disposition == 1 ? "Required condition" : "Preserved but inactive",
                                                  systemImage: finding.disposition == 1 ? "exclamationmark.shield" : "archivebox")
                                                .font(.caption.bold())
                                            Text(verbatim: "\(finding.entityID) · \(finding.setting)").font(.caption)
                                            Text(verbatim: finding.reason).font(.caption)
                                        }
                                    }
                                }.padding(.top, 8)
                            }
                        }
                        Text("Swipe to inspect another machine. These are saved configurations, not live panes.").font(.caption)
                    }
                    .padding(28).padding(.bottom, 24).frame(maxWidth: .infinity, alignment: .leading)
                }.frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
                    .foregroundStyle(Color(ftHex: theme?.foreground ?? "#FFFFFF"))
                    .background(Color(ftHex: theme?.background ?? "#10223C"))
                    .tag(machine.id)
            }
        }.tabViewStyle(.page(indexDisplayMode: .always))
            .navigationTitle(document.name).navigationBarTitleDisplayMode(.inline)
            .onAppear { if selection.isEmpty { selection = document.initialMachineID } }
            .toolbar {
                Menu("Groups", systemImage: "rectangle.3.group") {
                    ForEach(document.groups) { group in
                        Button {
                            groupID = group.id
                            if !group.machineOrder.contains(selection) { selection = group.machineOrder.first ?? "" }
                        } label: { Text(verbatim: group.name) }
                    }
                }
                Menu("Machines", systemImage: "list.bullet") {
                    ForEach(machines) { machine in Button { selection = machine.id } label: { Text(verbatim: machine.name) } }
                }
            }
    }
}

private extension Color {
    init(ftHex: String) {
        let raw = String(ftHex.dropFirst())
        let number = UInt64(raw, radix: 16) ?? 0
        let rgba = raw.count == 8 ? number : (number << 8) | 255
        self.init(.sRGB, red: Double((rgba >> 24) & 255) / 255, green: Double((rgba >> 16) & 255) / 255,
                  blue: Double((rgba >> 8) & 255) / 255, opacity: Double(rgba & 255) / 255)
    }
}
