import SwiftUI
import Observation
import VisionKit
import AVFoundation
import UniformTypeIdentifiers
import UIKit
import FrankenTermKit
import FrankenTermSSHIntegration

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
    var localPolicies: [UUID: WorkspaceLocalState] = [:]
    var state: OfflineRestoreStatus? { didSet { updateIdleTimer() } }
    var message: String?
    var showRestore = false { didSet { updateIdleTimer() } }
    var cameraRequested = false { didSet { updateIdleTimer() } }
    var active = false { didSet { updateIdleTimer() } }
    var busy = false
    var requestingCameraAccess = false
    var offlineModeBlocked = false
    @ObservationIgnored private var store: WorkspaceStore?
    /// Live direct-SSH services (TerminalSessionScreen.swift), created with the store.
    @ObservationIgnored private(set) var connections: MobileConnections?
    @ObservationIgnored private var connectionLifecycle: Task<Void, Never>?
    @ObservationIgnored private var restore: OfflineRestore<AppleCryptography>?
    @ObservationIgnored private var bootstrapStarted = false
    private var pendingImport: PendingFileImport?
    @ObservationIgnored private var epoch = UUID()
    @ObservationIgnored private var lifecycle: Task<Void, Never>?
    @ObservationIgnored private var workspaceRefresh: Task<Void, Never>?
    @ObservationIgnored private var observationTask: Task<Void, Never>?
    @ObservationIgnored private var observations: AsyncStream<CameraObservation>.Continuation?
    @ObservationIgnored private var lastObservation: String?
    @ObservationIgnored private var lastObservationTime = ContinuousClock.now
    @ObservationIgnored private var previousIdleTimerSetting: Bool?
    private struct CameraObservation: Sendable { let text: String, generation: UUID, epoch: UUID }
    private struct PendingFileImport: Sendable {
        let id = UUID()
        let url: URL
        var needsExplicitRetry = false
    }
    var hasPendingImport: Bool { pendingImport != nil }
    var pendingImportNeedsRetry: Bool { pendingImport?.needsExplicitRetry == true }

    private func updateIdleTimer() {
        let needsAwakeDisplay = active && showRestore && state?.isSuspended == false
            && (cameraRequested || state?.recipientRequest != nil)
        if needsAwakeDisplay {
            if previousIdleTimerSetting == nil {
                previousIdleTimerSetting = UIApplication.shared.isIdleTimerDisabled
            }
            UIApplication.shared.isIdleTimerDisabled = true
        } else if let previousIdleTimerSetting {
            UIApplication.shared.isIdleTimerDisabled = previousIdleTimerSetting
            self.previousIdleTimerSetting = nil
        }
    }

    func requestCamera() {
        guard active, !busy, !requestingCameraAccess, state?.requiresRecipientResume != true else { return }
        if !showRestore { setRestoreVisible(true) }
        requestingCameraAccess = true
        let ticket = epoch, pendingLifecycle = lifecycle
        Task { [weak self] in
            // Offline admission precedes both the camera permission prompt and
            // scanner creation, including the root screen's one-tap entry.
            await pendingLifecycle?.value
            guard let self else { return }
            defer { self.requestingCameraAccess = false }
            guard self.active, self.showRestore, self.epoch == ticket, !self.offlineModeBlocked,
                  self.state?.isSuspended == false else { return }
            guard DataScannerViewController.isSupported else {
                self.message = "This device does not support the native scanner. Open a local encrypted file instead."
                return
            }
            let status = AVCaptureDevice.authorizationStatus(for: .video)
            if status == .authorized { self.cameraRequested = true; return }
            guard status == .notDetermined else {
                self.message = "Camera access is denied or restricted. Allow it in Settings or use a local encrypted file."
                return
            }
            let granted = await AVCaptureDevice.requestAccess(for: .video)
            guard self.showRestore else { return }
            // The permission sheet can deactivate the scene. Do not restart a
            // camera in an inactive scene or after the restore sheet was closed.
            if granted, self.active, self.epoch == ticket, !self.offlineModeBlocked,
               self.state?.isSuspended == false { self.cameraRequested = true }
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
            if let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first {
                connections = try MobileConnections(store: created,
                    journalDirectory: support.appendingPathComponent("FrankenTermSessions", isDirectory: true))
                updateConnectionLifecycle()
            }
            try await refreshWorkspaces(using: created, epoch: epoch)
            if active, pendingImport != nil { showRestore = true }
            updateLifecycle()
        } catch { report(error); bootstrapStarted = false }
    }
    func setActive(_ value: Bool) {
        active = value
        if !value {
            cameraRequested = false; state = nil; availableCredentials = [:]; localPolicies = [:]
            // A system permission sheet can interrupt a read. Do not turn its
            // foreground callback into another automatic prompt/read attempt.
            if busy { pendingImport?.needsExplicitRetry = true }
        } else if pendingImport != nil { showRestore = true }
        updateLifecycle()
        updateConnectionLifecycle()
    }
    /// Serialized so a quick background/foreground pair reaches the connection
    /// manager in order; it seals and settles live sessions when inactive.
    private func updateConnectionLifecycle() {
        guard let connections else { return }
        let prior = connectionLifecycle, value = active
        connectionLifecycle = Task {
            await prior?.value
            await connections.setActive(value)
        }
    }
    func setRestoreVisible(_ value: Bool) {
        showRestore = value
        // A just-dismissed request may already be retired. Re-present only the
        // status returned by the serialized lifecycle transition.
        state = nil
        if !value { cameraRequested = false; pendingImport = nil; offlineModeBlocked = false }
        updateLifecycle(discardRecipientRequest: !value)
    }
    func retryOfflineMode() {
        guard active, showRestore, !busy else { return }
        message = nil
        updateLifecycle()
    }
    func requestRecipient() {
        guard active, !busy else { return }
        pendingImport = nil; cameraRequested = false; setRestoreVisible(true)
        run { restore, state in
            let current = state.phase == .complete ? try await restore.reset() : state
            return try await restore.createRecipientRequest(generation: current.generation)
        }
    }
    func resumeRecipientRequest() {
        guard active, !busy else { return }
        pendingImport?.needsExplicitRetry = false
        run { restore, state in try await restore.resumeRecipientRequest(generation: state.generation) }
    }
    func resetRestore() {
        guard active, !busy else { return }
        pendingImport = nil; cameraRequested = false
        run { restore, _ in try await restore.reset() }
    }
    func retryPendingImport() {
        guard active, !busy, state?.isSuspended == false else { return }
        pendingImport?.needsExplicitRetry = false
        openPendingFileIfReady()
    }
    private func updateLifecycle(discardRecipientRequest: Bool = false) {
        epoch = UUID()
        workspaceRefresh?.cancel()
        observations?.finish(); observations = nil
        observationTask?.cancel(); observationTask = nil; lastObservation = nil
        guard let restore else { return }
        let ticket = epoch, visible = active && showRestore, prior = lifecycle
        lifecycle = Task { [weak self] in
            await prior?.value
            let next: OfflineRestoreStatus
            do {
                if visible {
                    try await restore.enterOfflineMode()
                    next = await restore.resume()
                } else { next = await restore.suspend(discardRecipientRequest: discardRecipientRequest) }
            } catch {
                let paused = await restore.suspend()
                guard let self, self.epoch == ticket else { return }
                self.cameraRequested = false; self.offlineModeBlocked = true
                self.state = paused; self.report(error)
                return
            }
            guard let self, self.epoch == ticket else { return }
            self.offlineModeBlocked = false
            self.state = next
            if visible { self.startObservations(restore: restore) }
            if self.active, let store = self.store {
                self.scheduleWorkspaceRefresh(using: store, epoch: ticket)
            }
            guard self.epoch == ticket else { return }
            self.openPendingFileIfReady()
        }
    }
    private func scheduleWorkspaceRefresh(using store: WorkspaceStore, epoch ticket: UUID) {
        let prior = workspaceRefresh
        prior?.cancel()
        // Protected-storage reads must not hold the transition chain that
        // suspends collection or retires a dismissed request. Serialize these
        // refreshes separately and fence their eventual public UI results.
        workspaceRefresh = Task { [weak self] in
            await prior?.value
            guard !Task.isCancelled, let self, self.active, self.epoch == ticket else { return }
            do { try await self.refreshWorkspaces(using: store, epoch: ticket) }
            catch {
                guard !Task.isCancelled, self.active, self.epoch == ticket else { return }
                self.availableCredentials = [:]; self.localPolicies = [:]; self.report(error)
            }
        }
    }
    private func refreshWorkspaces(using store: WorkspaceStore, epoch ticket: UUID) async throws {
        let snapshot = try await store.loadWithLocalPolicy(support: WorkspaceNativeSSHAuthentication.routeSupport)
        guard active, epoch == ticket else { return }
        workspaces = snapshot.map(\.workspace)
        availableCredentials = Dictionary(uniqueKeysWithValues: snapshot.map { ($0.workspace.id, $0.credentialIDs) })
        localPolicies = Dictionary(uniqueKeysWithValues: snapshot.map { ($0.workspace.id, $0) })
        // A first import, or new machines in a replaced one: machines that
        // look alike get distinct themes before anything shows them.
        for saved in workspaces {
            if let document = try? saved.document(), let themes = machineThemes(saved.id, document: document) {
                setThemeChoices(themes.distinguishingMachines(), for: saved.id)
            }
        }
    }

    /// Every theme a machine can wear, bundled from `ft mobile themes`.
    static let themeCatalog: ThemeCatalog? = Bundle.main.url(forResource: "ThemeCatalog", withExtension: "json")
        .flatMap { try? Data(contentsOf: $0) }.flatMap { try? ThemeCatalog(data: $0) }
    /// Bumped on every theme change so views read the choices again.
    private(set) var themeRevision = 0
    private static func themeKey(_ workspaceID: UUID) -> String { "machine-themes.\(workspaceID.uuidString)" }

    /// This phone's theme choices for a workspace's machines. They are not
    /// secret, so they live in user defaults, not the protected store.
    func themeChoices(_ workspaceID: UUID) -> [String: MachineThemeChoice] {
        _ = themeRevision
        guard let data = UserDefaults.standard.data(forKey: Self.themeKey(workspaceID)) else { return [:] }
        return (try? JSONDecoder().decode([String: MachineThemeChoice].self, from: data)) ?? [:]
    }
    func machineThemes(_ workspaceID: UUID, document: WorkspaceDocument) -> MachineThemes? {
        Self.themeCatalog.map { MachineThemes(document: document, catalog: $0, choices: themeChoices(workspaceID)) }
    }
    /// The user's pick, which no automatic choice ever replaces.
    func chooseTheme(_ choice: MachineThemeChoice, machineID: String, workspaceID: UUID) {
        var choices = themeChoices(workspaceID)
        choices[machineID] = MachineThemeChoice(name: choice.name, catalog: choice.catalog)
        setThemeChoices(choices, for: workspaceID)
    }
    private func setThemeChoices(_ choices: [String: MachineThemeChoice], for workspaceID: UUID) {
        guard choices != themeChoices(workspaceID), let data = try? JSONEncoder().encode(choices) else { return }
        UserDefaults.standard.set(data, forKey: Self.themeKey(workspaceID))
        themeRevision += 1
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
                catch {
                    let latest = await restore.status()
                    guard self.active, self.epoch == observation.epoch else { continue }
                    if self.state?.generation != latest.generation { self.lastObservation = nil }
                    self.state = latest
                    if ![.idle, .candidate, .collecting].contains(latest.phase) { self.cameraRequested = false }
                    self.report(error)
                }
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
            defer {
                self.busy = false
                self.openPendingFileIfReady()
            }
            guard self.active, self.epoch == ticket, !self.offlineModeBlocked else { return }
            do {
                let current = await restore.status()
                let result = try await action(restore, current)
                guard self.active, self.epoch == ticket else { return }
                if current.generation != result.generation { self.lastObservation = nil }
                self.state = result
                if result.phase == .complete, let store = self.store { try await self.refreshWorkspaces(using: store, epoch: ticket) }
            } catch {
                let latest = await restore.status()
                guard self.active, self.epoch == ticket else { return }
                self.state = latest
                self.report(error)
            }
        }
    }
    func importFile(_ url: URL) {
        guard url.isFileURL else { report(FTError.malformed("file.local_url_required")); return }
        guard !busy else { message = "Finish the current import before opening another workspace."; return }
        pendingImport = PendingFileImport(url: url)
        guard active, restore != nil else {
            if !bootstrapStarted { Task { await bootstrap() } }
            return
        }
        cameraRequested = false; setRestoreVisible(true)
        // The serialized lifecycle calls openPendingFileIfReady after obtaining
        // the actor's current status. A paused phone request keeps its selection.
    }
    private func openPendingFileIfReady() {
        guard active, showRestore, !busy, let restore, let selected = pendingImport,
              !selected.needsExplicitRetry, state?.isSuspended == false else { return }
        busy = true; message = nil
        let ticket = epoch, pendingLifecycle = lifecycle
        Task { [weak self] in
            await pendingLifecycle?.value
            guard let self else { return }
            defer { self.busy = false }
            guard self.active, self.showRestore, self.epoch == ticket, self.pendingImport?.id == selected.id else {
                self.markPendingImportInterrupted(selected.id)
                return
            }
            let current = await restore.status()
            guard self.active, self.epoch == ticket, self.pendingImport?.id == selected.id else {
                self.markPendingImportInterrupted(selected.id)
                return
            }
            self.state = current
            guard !current.isSuspended else { return }
            var submittedDocument = false
            do {
                let ready = current.phase == .complete ? try await restore.reset() : current
                guard ready.phase == .idle else { throw WorkspaceStoreError.busy }
                guard self.active, self.epoch == ticket, self.pendingImport?.id == selected.id else {
                    self.markPendingImportInterrupted(selected.id)
                    return
                }
                let bytes = try await Task.detached(priority: .userInitiated) {
                    try SelectedLocalFile.read(selected.url, limit: FTLimit.envelope)
                }.value
                guard self.active, self.epoch == ticket, self.pendingImport?.id == selected.id else {
                    self.markPendingImportInterrupted(selected.id)
                    return
                }
                let result: OfflineRestoreStatus
                if bytes.starts(with: Data("FTAG".utf8)) {
                    result = try await restore.acceptEncryptedFile(bytes, generation: ready.generation)
                } else {
                    submittedDocument = true
                    result = try await restore.acceptWorkspaceDocument(bytes, generation: ready.generation)
                }
                // Acceptance owns the selected input even if a later suspension
                // prevents publishing its result to this view. Never replay it.
                if self.pendingImport?.id == selected.id { self.pendingImport = nil }
                guard self.active, self.epoch == ticket else { return }
                self.state = result
            } catch {
                let latest = await restore.status()
                guard self.pendingImport?.id == selected.id else { return }
                let interrupted = !self.active || self.epoch != ticket || latest.isSuspended
                // A public document can be admitted before its awaited preview
                // is suspended. The actor already owns it and offers fresh review.
                let ownsDocument = submittedDocument && latest.source == .workspaceDocument
                    && latest.phase != .idle && latest.phase != .complete
                if ownsDocument || !interrupted { self.pendingImport = nil }
                else { self.markPendingImportInterrupted(selected.id) }
                guard self.active, self.epoch == ticket else { return }
                self.state = latest
                if !interrupted { self.report(error) }
            }
        }
    }
    private func markPendingImportInterrupted(_ id: UUID) {
        guard pendingImport?.id == id else { return }
        pendingImport?.needsExplicitRetry = true
    }
    func report(_ error: Error) {
        if let error = error as? FTError {
            if error == .authentication, state?.encryptionMode == .recipient || state?.recipientRequest != nil {
                message = "This transfer could not be verified for the active phone request. Check the recipient value on both screens and create a new export on the Mac."
            } else if error == .limit("recipient.envelope_limit") {
                message = "This transfer exceeds the phone request's accepted size. Select fewer groups or identities on the Mac, then prepare a new transfer."
            } else if error == .unsupported("envelope.requires_recipient_key"), state?.recipientRequest != nil {
                message = "This receive session is waiting for an export encrypted to its phone request. Ask the Mac to use Only This Phone, or explicitly start a different transfer to use a code."
            } else { message = error.localizedDescription }
        }
        else if let error = error as? WorkspaceStoreError { message = error.localizedDescription }
        else if let error = error as? WorkspaceNetworkError { message = error.localizedDescription }
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
                        Text("Restore your Mac workspace from an encrypted transfer or a public workspace file. Included private keys go into this device’s protected Keychain.")
                            .foregroundStyle(.secondary)
                        Button { model.requestCamera() } label: {
                            Label("Restore from Animated QR", systemImage: "qrcode.viewfinder")
                        }.disabled(model.state == nil)
                        Button { model.requestRecipient() } label: {
                            Label("Encrypt Only for This Phone", systemImage: "iphone.and.arrow.forward")
                        }.disabled(model.state == nil || model.busy || ![.idle, .complete].contains(model.state?.phase ?? .idle))
                        Button { chooseFile = true } label: { Label("Open Workspace File", systemImage: "doc.badge.arrow.up") }
                            .disabled(model.state == nil || model.busy)
                    }
                    Section("Saved workspaces") {
                        if model.workspaces.isEmpty { Text("No workspace restored yet.").foregroundStyle(.secondary) }
                        ForEach(model.workspaces) { saved in
                            if let document = try? saved.document() {
                                NavigationLink { MachineWorkspaceView(initialDocument: document, workspaceID: saved.id, model: model) } label: {
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
                        Text("Development build: offline workspace restoration and direct SSH terminals (Connect on a restored machine). Persistent FrankenTerm mux sessions are not integrated yet.")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                }.navigationTitle("FrankenTerm")
            }
            if !model.active { Color(.systemBackground).ignoresSafeArea().overlay { Label("FrankenTerm", systemImage: "lock.shield").font(.title2) } }
        }
        .task { model.setActive(scenePhase == .active); await model.bootstrap() }
        .onChange(of: scenePhase) { _, phase in model.setActive(phase == .active) }
        .onOpenURL { model.importFile($0) }
        .sheet(isPresented: Binding(get: { model.showRestore }, set: { model.setRestoreVisible($0) })) {
            OfflineRestoreSheet(model: model)
        }
        .fileImporter(isPresented: $chooseFile, allowedContentTypes: [.json, .data], allowsMultipleSelection: false) { result in
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
    @State private var associateWithExistingWorkspace = false
    @State private var securityChangesConfirmed = false
    @State private var repairPage = 0
    @State private var chooseFile = false
    var body: some View {
        NavigationStack {
            Group {
                if !model.active { ContentUnavailableView("Locked", systemImage: "lock.shield", description: Text("Return to FrankenTerm to continue.")) }
                else if model.offlineModeBlocked {
                    ContentUnavailableView {
                        Label("Offline Restore Paused", systemImage: "network.slash")
                    } description: {
                        Text(model.message ?? "Close active connections before restoring. Cancel to keep your connections open.")
                    } actions: {
                        Button("Retry Offline Restore") { model.retryOfflineMode() }.buttonStyle(.borderedProminent)
                        Button("Cancel Restore") { model.setRestoreVisible(false) }
                    }
                }
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
                                    Text(state.source == .workspaceDocument ? "File comparison value" : "Compare with your Mac").font(.caption).foregroundStyle(.secondary)
                                    Text(verbatim: comparison).font(.system(.headline, design: .monospaced)).minimumScaleFactor(0.6).lineLimit(1)
                                }.padding().frame(maxWidth: .infinity, alignment: .leading).background(.quaternary, in: RoundedRectangle(cornerRadius: 16))
                            }
                            if state.requiresRecipientResume {
                                Label("Phone request paused", systemImage: "lock.shield").font(.headline)
                                Text("Your request key is still held only in this running app. Resume this request deliberately before scanning or decrypting. Closing this restore or terminating the app loses the key.")
                                if model.hasPendingImport {
                                    Text("Your selected local file is waiting. It will open after you resume this phone request.")
                                }
                                Button("Resume This Phone Request") {
                                    model.resumeRecipientRequest()
                                }.buttonStyle(.borderedProminent)
                            } else {
                            if model.pendingImportNeedsRetry {
                                Text("The selected file is still waiting after an interruption. Continue when you are ready; the app will not reopen it automatically.")
                                Button("Continue Selected File") { model.retryPendingImport() }.buttonStyle(.borderedProminent)
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
                                if let request = state.recipientRequest {
                                    RecipientRequestCard(request: request)
                                    Text("On your Mac, choose Only This Phone and scan this QR. Compare the recipient value on both devices before exporting. Then point this phone at the Mac’s encrypted transfer.")
                                    Button("Scan the Mac’s Encrypted Transfer") { model.requestCamera() }.disabled(model.requestingCameraAccess)
                                } else {
                                    Text("Point at the Mac’s Offline QR Transfer. No network service is used.")
                                    Button("Open Camera") { model.requestCamera() }.disabled(model.requestingCameraAccess)
                                    Button("Encrypt Only for This Phone") { model.requestRecipient() }
                                }
                                Button(state.recipientRequest == nil ? "Open Local Workspace File" : "Open Local Encrypted File") {
                                    chooseFile = true
                                }
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
                                if !state.incompletePageIndices.isEmpty {
                                    DisclosureGroup("Repair an unfinished page") {
                                        Text("Select the same page in Repair Page on your Mac. Reset only if this page has stopped recovering; verified pages are kept.")
                                            .font(.caption).foregroundStyle(.secondary)
                                        Picker("Page", selection: $repairPage) {
                                            ForEach(state.incompletePageIndices, id: \.self) { index in
                                                Text("Page \(index + 1)").tag(index)
                                            }
                                        }
                                        Button("Reset This Unfinished Page") {
                                            let page = state.incompletePageIndices.contains(repairPage) ? repairPage : (state.incompletePageIndices.first ?? 0)
                                            model.run { restore, state in try await restore.resetIncompletePage(page, generation: state.generation) }
                                        }
                                    }
                                }
                            case .locked:
                                if state.encryptionMode == .recipient {
                                    if state.canUnlockRecipient {
                                        Text("The complete package is encrypted for this phone request. Decrypt it, then compare the envelope value on your Mac and review the source and contents.")
                                        Button("Decrypt for This Phone and Review") {
                                            confirmed = false; allowRemoval = false; allowRollback = false
                                            model.run { restore, state in try await restore.unlockRecipient(generation: state.generation) }
                                        }.buttonStyle(.borderedProminent)
                                    } else {
                                        Text("This package needs a matching private phone request that is not available in this session. Start a different transfer, create a fresh phone request, and have the Mac encrypt again. A transfer code cannot unlock this package.")
                                    }
                                } else if state.recipientRequest != nil {
                                    Text("This package uses a transfer code, but you requested encryption only for this phone. Ask the Mac to use the confirmed phone request, or start a different transfer to choose code mode explicitly.")
                                } else {
                                Text("The complete encrypted package is collected. Reveal the transfer code separately on your Mac and enter it here.")
                                SecureField("Transfer code", text: $code).textInputAutocapitalization(.characters).autocorrectionDisabled()
                                    .textFieldStyle(.roundedBorder).onChange(of: code) { _, value in if value.utf8.count > 128 { code = String(value.prefix(128)) } }
                                Button("Decrypt and Review") {
                                    let entered = code; code = ""; confirmed = false; allowRemoval = false; allowRollback = false
                                    model.run { restore, state in try await restore.unlock(enteredCode: entered, generation: state.generation) }
                                }.buttonStyle(.borderedProminent).disabled(code.isEmpty)
                                }
                            case .documentReady:
                                Text("The public workspace is ready for a fresh source and settings review. It contains no private keys and needs no transfer code.")
                                Button("Review Workspace") {
                                    model.run { restore, state in try await restore.reviewWorkspaceDocument(generation: state.generation) }
                                }.buttonStyle(.borderedProminent)
                            case .preparingReview, .committing:
                                ProgressView(state.phase == .committing ? "Saving protected workspace…" : "Preparing review…")
                            case .review:
                                if let review = state.review {
                                    Text(verbatim: review.document.name).font(.headline)
                                    Text("\(review.document.machines.count) machines · \(review.includedFingerprints.count) private identities included")
                                    if state.source == .workspaceDocument {
                                        Text("This public document is not encrypted or authenticated. Review its source, endpoints, and host trust before accepting it. Existing matching device credentials can be reused.")
                                            .font(.callout)
                                    } else if state.encryptionMode == .recipient {
                                        Text("Encryption to this phone does not identify the sender. Compare this envelope’s value with the trusted Mac before accepting host policy or private keys.")
                                            .font(.callout)
                                    }
                                    ForEach(review.document.machines) { machine in
                                        let theme = review.document.themes.first { $0.id == machine.themeID }
                                        HStack {
                                            Circle().fill(Color(ftHex: theme?.accent ?? "#777777")).frame(width: 12, height: 12)
                                            Text(verbatim: machine.name).lineLimit(1)
                                        }
                                        ForEach(review.document.routes.filter { machine.routeIDs.contains($0.id) }) { route in
                                            VStack(alignment: .leading, spacing: 3) {
                                                Text(verbatim: "\(route.user)@\(route.host):\(route.port)").font(.system(.caption, design: .monospaced))
                                                Text("\(route.credentialIDs.count) permitted identities · \(route.authenticationMethods.map(\.rawValue).joined(separator: ", "))")
                                                    .font(.caption).foregroundStyle(.secondary)
                                                if let trust = review.document.hostTrust.first(where: { $0.id == route.trustID }) {
                                                    ForEach(trust.pins, id: \.self) { key in
                                                        Text(verbatim: "Host \(key.algorithm) \(key.fingerprint)").font(.system(.caption2, design: .monospaced))
                                                    }
                                                    if !trust.revokedKeys.isEmpty { Text("\(trust.revokedKeys.count) host-key revocations apply.").font(.caption) }
                                                }
                                            }
                                        }
                                    }
                                    ForEach(Array(review.includedFingerprints.enumerated()), id: \.offset) { _, fingerprint in
                                        Text(verbatim: fingerprint).font(.system(.caption2, design: .monospaced)).textSelection(.enabled)
                                    }
                                    ImportHostPolicyReview(document: review.document, routeReadiness: review.routeReadiness)
                                    if let changes = review.changeReview {
                                        WorkspaceReplacementReview(changes: changes,
                                            existingName: review.existingWorkspaceName ?? review.document.name)
                                    }
                                    if review.inactiveSettingCount > 0 { Text("\(review.inactiveSettingCount) desktop-only settings are preserved but inactive.") }
                                    ForEach(Array(review.findings.enumerated()), id: \.offset) { _, finding in
                                        VStack(alignment: .leading) {
                                            Text(verbatim: finding.setting).font(.caption.bold())
                                            Text(verbatim: finding.reason).font(.caption)
                                        }
                                    }
                                    if review.preservedHostRevocationCount > 0 {
                                        Text("\(review.preservedHostRevocationCount) previously accepted host-key denials remain in force.").font(.caption)
                                    }
                                    if review.preservedHostRequirementCount > 0 {
                                        Text("\(review.preservedHostRequirementCount) mandatory host-trust requirements remain in force, even if this export omits them.").font(.caption)
                                    }
                                    if !review.document.diagnostics.isEmpty {
                                        Text("Unknown optional extensions were discarded. Only validated public settings will be saved.").font(.caption)
                                    }
                                    Toggle(state.source == .workspaceDocument
                                           ? "I recognize this source and approve the reviewed settings and host trust."
                                           : "I verified the comparison value on my Mac and approve these settings, trust records, and keys.", isOn: $confirmed)
                                    if review.requiresSourceAssociation {
                                        VStack(alignment: .leading, spacing: 8) {
                                            Label("This export claims a saved workspace", systemImage: "arrow.triangle.branch").font(.headline)
                                            Text("Matching workspace IDs and successful decryption do not prove the same Mac created this export. Continue only after verifying its source. This will replace the saved workspace shown above; cancel to keep it.")
                                                .font(.callout)
                                            Toggle("I associate this source with the saved workspace and approve replacing its settings.",
                                                   isOn: $associateWithExistingWorkspace)
                                        }.padding().background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
                                    }
                                    if review.requiresSecurityApproval {
                                        Toggle("I reviewed and approve the highlighted connection, credential, and host-trust changes.",
                                               isOn: $securityChangesConfirmed)
                                    }
                                    if review.removesMachines { Toggle("Replace this workspace even though some existing machines are removed.", isOn: $allowRemoval) }
                                    if review.isRollback {
                                        VStack(alignment: .leading, spacing: 8) {
                                            Label("Previously restored recording", systemImage: "clock.arrow.circlepath").font(.headline)
                                            Text("This package was accepted before and differs from the workspace currently saved on this phone. Restoring it replaces the current settings. Local revocations and mandatory host-trust policy stay in effect.")
                                                .font(.callout)
                                            Toggle("I approve restoring this previously accepted workspace version.", isOn: $allowRollback)
                                        }.padding().background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
                                    }
                                    Text("No connection or remote command will run. A device passcode is required for private-key storage.").font(.footnote).foregroundStyle(.secondary)
                                    Button("Restore Offline") {
                                        let token = review.token, consent = confirmed, removal = allowRemoval, rollback = allowRollback
                                        let association = associateWithExistingWorkspace, security = securityChangesConfirmed
                                        model.run { restore, state in
                                            try await restore.commit(reviewToken: token, generation: state.generation,
                                                                     sourceAndContentsConfirmed: consent, allowMachineRemoval: removal, allowRollback: rollback,
                                                                     associateWithExistingWorkspace: association, securityChangesConfirmed: security)
                                        }
                                    }.buttonStyle(.borderedProminent).disabled(!confirmed || (review.removesMachines && !allowRemoval)
                                        || (review.isRollback && !allowRollback)
                                        || (review.requiresSourceAssociation && !associateWithExistingWorkspace)
                                        || (review.requiresSecurityApproval && !securityChangesConfirmed))
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
                            }
                            if ![.committing, .preparingReview, .recoveryRequired].contains(state.phase) {
                                Button("Start a Different Transfer", role: .destructive) {
                                    code = ""; confirmed = false; allowRemoval = false; allowRollback = false; model.cameraRequested = false
                                    associateWithExistingWorkspace = false; securityChangesConfirmed = false
                                    model.resetRestore()
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
        .fileImporter(isPresented: $chooseFile, allowedContentTypes: [.json, .data], allowsMultipleSelection: false) { result in
            switch result {
            case .success(let urls): if let url = urls.first { model.importFile(url) }
            case .failure(let error): model.report(error)
            }
        }
        .onChange(of: model.active) { _, active in
            if !active {
                code = ""; confirmed = false; allowRemoval = false; allowRollback = false
                associateWithExistingWorkspace = false; securityChangesConfirmed = false
            }
        }
        .onChange(of: model.state?.review?.token) { _, _ in
            confirmed = false; allowRemoval = false; allowRollback = false
            associateWithExistingWorkspace = false; securityChangesConfirmed = false
        }
        .onDisappear {
            code = ""; confirmed = false; allowRemoval = false; allowRollback = false; model.cameraRequested = false
            associateWithExistingWorkspace = false; securityChangesConfirmed = false
        }
    }
    private func title(_ phase: OfflineRestorePhase) -> String {
        switch phase {
        case .idle: "Scan your Mac"
        case .candidate: "Choose this source"
        case .collecting: "Collecting encrypted pages"
        case .locked: "Unlock the package"
        case .documentReady, .preparingReview, .review: "Review your workspace"
        case .committing: "Restoring workspace"
        case .complete: "Your workspace is restored"
        case .recoveryRequired: "Check the save result"
        }
    }
}

@MainActor
private struct RecipientRequestCard: View {
    let request: RecipientRequest
    @State private var image: UIImage?
    @State private var failed = false
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Only this phone request").font(.headline)
            if let image {
                Image(uiImage: image).resizable().interpolation(.none)
                    .frame(width: 226, height: 226).background(.white)
                    .accessibilityLabel("Public recipient request QR. Compare the value below on your Mac.")
            } else if failed {
                Text("The phone request QR could not be rendered. Start a different transfer and create a new request.")
            } else { ProgressView("Preparing public request…") }
            Text("Recipient comparison").font(.caption).foregroundStyle(.secondary)
            Text(verbatim: request.comparison(using: AppleCryptography()))
                .font(.system(.headline, design: .monospaced)).minimumScaleFactor(0.6).lineLimit(1)
            Text("The request is public. Its private key stays in this app’s memory. Closing this restore or terminating the app loses that key; the Mac must then create a new export. A recording for this request is not a recoverable backup.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .task(id: request.encoded) {
            image = nil; failed = false
            do {
                let symbol = try await NativeOpticalPresentation().recipientSymbol(text: request.text)
                try Task.checkCancellation()
                guard let decoded = UIImage(data: symbol.png) else { throw FTError.storage }
                image = decoded
            } catch is CancellationError { }
            catch { failed = true }
        }
    }
}

@MainActor
private struct WorkspaceReplacementReview: View {
    let changes: WorkspaceChangeReview
    let existingName: String
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Changes to your saved workspace").font(.headline)
            Text(verbatim: existingName).font(.subheadline)
            if changes.changes.isEmpty {
                Text("The public workspace settings are unchanged. The export identity and available private keys may differ.")
                    .font(.callout)
            } else {
                Text("\(changes.changes.count) changed fields · \(changes.securityChanges.count) affect connection or trust policy")
                    .font(.callout)
                DisclosureGroup("Review every previous and incoming value") {
                    LazyVStack(alignment: .leading, spacing: 16) {
                        ForEach(Array(changes.changes.enumerated()), id: \.offset) { _, change in
                            VStack(alignment: .leading, spacing: 5) {
                                if change.affectsSecurity {
                                    Label("Connection or trust change", systemImage: "exclamationmark.shield").font(.caption.bold())
                                }
                                Text(verbatim: "\(change.entity.title) · \(change.entityID)").font(.caption.bold())
                                Text(verbatim: change.fieldTitle).font(.caption)
                                Text("Saved value").font(.caption2).foregroundStyle(.secondary)
                                Text(verbatim: WorkspaceChangeReview.Change.description(of: change.previous))
                                    .font(.system(.caption2, design: .monospaced)).textSelection(.enabled)
                                Text("Incoming value").font(.caption2).foregroundStyle(.secondary)
                                Text(verbatim: WorkspaceChangeReview.Change.description(of: change.incoming))
                                    .font(.system(.caption2, design: .monospaced)).textSelection(.enabled)
                            }.frame(maxWidth: .infinity, alignment: .leading)
                            Divider()
                        }
                    }.padding(.top, 8)
                }
            }
            Text("Previously accepted local revocations and mandatory host-trust requirements remain in force.")
                .font(.caption).foregroundStyle(.secondary)
        }.padding().background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
    }
}

@MainActor
private struct ImportHostPolicyReview: View {
    let document: WorkspaceDocument
    let routeReadiness: [WorkspaceDocument.RouteReadiness]
    var body: some View {
        DisclosureGroup("All host identities and jump-route policy") {
            VStack(alignment: .leading, spacing: 16) {
                ForEach(document.hostTrust) { trust in
                    VStack(alignment: .leading, spacing: 5) {
                        Text(verbatim: "\(trust.host):\(trust.port)").font(.headline)
                        if let alias = trust.hostKeyAlias { Text(verbatim: "Host-key alias: \(alias)").font(.caption) }
                        ForEach(trust.pins, id: \.self) { pin in
                            Text(verbatim: "\(pin.algorithm) · \(pin.fingerprint)")
                                .font(.system(.caption2, design: .monospaced)).textSelection(.enabled)
                        }
                        if !trust.revokedKeys.isEmpty {
                            Text("\(trust.revokedKeys.count) explicitly revoked host identities remain blocked.").font(.caption)
                        }
                        ForEach(trust.requiredPolicyRequirements.sorted(), id: \.self) { requirement in
                            Text(verbatim: "Required policy: \(requirement)").font(.caption)
                        }
                    }
                }
                ForEach(document.routes) { route in
                    VStack(alignment: .leading, spacing: 5) {
                        Text(verbatim: "\(route.user)@\(route.host):\(route.port)").font(.headline)
                        Text("\(route.credentialIDs.count) permitted key references · \(route.jumpRoutes.count) jump routes").font(.caption)
                        if let state = routeReadiness.first(where: { $0.routeID == route.id }) {
                            ForEach(Array(state.issues.enumerated()), id: \.offset) { _, issue in
                                Label(issue.message, systemImage: "exclamationmark.circle").font(.caption)
                            }
                        } else {
                            Label("Local requirements could not be validated.", systemImage: "exclamationmark.shield").font(.caption)
                        }
                    }
                }
            }.padding(.top, 8)
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
    let initialDocument: WorkspaceDocument
    let workspaceID: UUID
    @Bindable var model: MobileWorkspaceModel
    @State private var selection = ""
    @State private var groupID = ""
    @State private var themeMachineID: String?
    private var document: WorkspaceDocument {
        (try? model.workspaces.first(where: { $0.id == workspaceID })?.document()) ?? initialDocument
    }
    /// The machine's theme on this phone: the user's pick, an automatic
    /// distinct one, or the workspace's.
    private func theme(for machine: WorkspaceDocument.Machine) -> WorkspaceDocument.Theme? {
        model.machineThemes(workspaceID, document: document)?.theme(for: machine)
            ?? document.themes.first { $0.id == machine.themeID }
    }
    private var machines: [WorkspaceDocument.Machine] {
        let selectedGroup = groupID.isEmpty ? document.initialGroupID : groupID
        let ids = document.groups.first(where: { $0.id == selectedGroup })?.machineOrder ?? []
        let map = Dictionary(uniqueKeysWithValues: document.machines.map { ($0.id, $0) })
        return ids.compactMap { map[$0] }
    }
    var body: some View {
        TabView(selection: $selection) {
            ForEach(machines) { machine in
                let theme = theme(for: machine)
                ScrollView {
                    VStack(alignment: .leading, spacing: 18) {
                        HStack { Image(systemName: "desktopcomputer"); Text(verbatim: machine.name).font(.largeTitle.bold()).lineLimit(2) }
                        HStack(spacing: 0) { ForEach(0..<16) { index in Color(ftHex: theme?.ansi[index] ?? "#000000") } }.frame(height: 24).clipShape(Capsule())
                        if MobileWorkspaceModel.themeCatalog != nil {
                            Button { themeMachineID = machine.id } label: {
                                Label(theme.map { "Theme: \($0.name)" } ?? "Choose Theme", systemImage: "paintpalette")
                            }.buttonStyle(.bordered).tint(Color(ftHex: theme?.accent ?? "#3B82F6"))
                                .accessibilityIdentifier("machine.theme")
                        }
                        Label("Restored offline", systemImage: "checkmark.shield")
                        Text("Inspect the saved routes and their local requirements below. Nothing connects until you tap Connect on a ready direct SSH route; the pinned host key is verified before any key is offered.").font(.callout)
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
                                    if let local = model.localPolicies[workspaceID] {
                                        Text("\(route.credentialIDs.filter(local.credentialIDs.contains).count) of \(route.credentialIDs.count) permitted key references are available on this device.")
                                            .font(.caption)
                                        if let readiness = local.routes.first(where: { $0.routeID == id }) {
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
                                    if route.mode == "direct_ssh", let connections = model.connections,
                                       let local = model.localPolicies[workspaceID],
                                       local.routes.first(where: { $0.routeID == id })?.issues.isEmpty == true,
                                       let credential = route.credentialIDs.first(where: local.credentialIDs.contains) {
                                        NavigationLink {
                                            liveWorkspace(connections: connections, local: local, initial: machine.id,
                                                          route: id, credential: credential)
                                        } label: { Label("Connect", systemImage: "terminal") }
                                            .buttonStyle(.borderedProminent)
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
                                                  systemImage: finding.disposition == 1 ? "exclamationmark.shield" : "archivebox").font(.caption.bold())
                                            Text(verbatim: "\(finding.entityID) · \(finding.setting)").font(.caption)
                                            Text(verbatim: finding.reason).font(.caption)
                                        }
                                    }
                                }.padding(.top, 8)
                            }
                        }
                        Text("Swipe to inspect another machine. These are saved configurations, not live panes.").font(.caption)
                    }.padding(28).padding(.bottom, 24).frame(maxWidth: .infinity, alignment: .leading)
                }.frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
                    .foregroundStyle(Color(ftHex: theme?.foreground ?? "#FFFFFF"))
                    .background(Color(ftHex: theme?.background ?? "#10223C"))
                    .tag(machine.id)
            }
        }.tabViewStyle(.page(indexDisplayMode: .always))
            .sheet(isPresented: Binding(get: { themeMachineID != nil }, set: { if !$0 { themeMachineID = nil } })) {
                if let id = themeMachineID, let machine = document.machines.first(where: { $0.id == id }),
                   let themes = model.machineThemes(workspaceID, document: document) {
                    MachineThemePicker(machine: machine, themes: themes) { choice in
                        model.chooseTheme(choice, machineID: id, workspaceID: workspaceID)
                    }
                }
            }
            .navigationTitle(document.name).navigationBarTitleDisplayMode(.inline)
            .onAppear { if selection.isEmpty { selection = document.initialMachineID } }
            .onChange(of: model.localPolicies[workspaceID]?.workspace.revision) { _, _ in
                if !document.groups.contains(where: { $0.id == groupID }) { groupID = document.initialGroupID }
                if !machines.contains(where: { $0.id == selection }) { selection = machines.first?.id ?? document.initialMachineID }
            }
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

    /// The live scene over this group's machines in their order. The chosen
    /// route opens first; any other machine can connect through its first
    /// ready direct-SSH route, preferred route first.
    private func liveWorkspace(connections: MobileConnections, local: WorkspaceLocalState,
                               initial: String, route: String, credential: String) -> some View {
        let themes = SceneThemeSource(
            themes: { [model, workspaceID, document] in model.machineThemes(workspaceID, document: document) },
            choose: { [model, workspaceID] machineID, choice in model.chooseTheme(choice, machineID: machineID, workspaceID: workspaceID) })
        return LiveWorkspaceView(initialMachineID: initial, themes: themes) {
            try WorkspaceTerminalScene(workspaceID: workspaceID, workspaceName: document.name,
                machines: machines.map { sceneMachine($0, local: local, chosen: $0.id == initial ? (route, credential) : nil) },
                initialMachineID: initial, connections: connections)
        }
    }

    private func sceneMachine(_ machine: WorkspaceDocument.Machine, local: WorkspaceLocalState,
                              chosen: (route: String, credential: String)?) -> SceneMachine {
        let theme = theme(for: machine)
        func scene(_ target: TerminalSessionTarget?) -> SceneMachine {
            .init(id: machine.id, name: machine.name, target: target,
                  unavailable: "None of this machine's direct SSH routes is ready on this device. Open the machine from the workspace to see what each route needs.",
                  theme: theme)
        }
        func target(_ route: String, _ credential: String) -> TerminalSessionTarget {
            .init(workspaceID: workspaceID, revision: local.workspace.revision, machineID: machine.id,
                  machineName: machine.name, routeID: route, credentialID: credential, theme: theme)
        }
        if let chosen { return scene(target(chosen.route, chosen.credential)) }
        var seen = Set<String>()
        for id in [machine.preferredRouteID] + machine.routeIDs where seen.insert(id).inserted {
            guard let route = document.routes.first(where: { $0.id == id }), route.mode == "direct_ssh",
                  local.routes.first(where: { $0.routeID == id })?.issues.isEmpty == true,
                  let credential = route.credentialIDs.first(where: local.credentialIDs.contains) else { continue }
            return scene(target(id, credential))
        }
        return scene(nil)
    }
}

extension Color {
    init(ftHex: String) {
        let raw = String(ftHex.dropFirst())
        let number = UInt64(raw, radix: 16) ?? 0
        let rgba = raw.count == 8 ? number : (number << 8) | 255
        self.init(.sRGB, red: Double((rgba >> 24) & 255) / 255, green: Double((rgba >> 16) & 255) / 255,
                  blue: Double((rgba >> 8) & 255) / 255, opacity: Double(rgba & 255) / 255)
    }
}
