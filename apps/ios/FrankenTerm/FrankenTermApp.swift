import SwiftUI
import Observation
import VisionKit
import AVFoundation
import UniformTypeIdentifiers
import UIKit
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration

@main
@MainActor
struct FrankenTermMobileApp: App {
    @State private var model = MobileWorkspaceModel()
    var body: some Scene {
        WindowGroup {
            MobileWorkspaceRoot(model: model).task { await Self.storageSelfTestIfRequested() }
        }
    }

    /// `devicectl device process launch --environment-variables
    /// '{"FT_STORAGE_SELF_TEST":"1"}'` runs a restore's save against a
    /// throwaway key and writes Documents/storage-self-test.txt.
    private static func storageSelfTestIfRequested() async {
        guard ProcessInfo.processInfo.environment["FT_STORAGE_SELF_TEST"] == "1" else { return }
        let files = FileManager.default
        let scratch = files.temporaryDirectory.appendingPathComponent("storage-self-test-" + UUID().uuidString)
        let report = await WorkspaceStorageSelfTest.run(directory: scratch)
        try? files.removeItem(at: scratch)
        guard let documents = files.urls(for: .documentDirectory, in: .userDomainMask).first else { return }
        try? Data((report.joined(separator: "\n") + "\n").utf8)
            .write(to: documents.appendingPathComponent("storage-self-test.txt"), options: .atomic)
    }
}

/// Confirmation names the exact retained owners it may close. A workspace
/// reopened or replaced while the dialog is visible requires a fresh review.
@MainActor
struct RestoreSessionDisconnectReview {
    let id = UUID()
    let workspaceNames: [String]
    fileprivate let epoch: UUID
    fileprivate let owners: [WorkspaceTerminalScene]

    fileprivate init(epoch: UUID, owners: [WorkspaceTerminalScene]) {
        self.epoch = epoch; self.owners = owners
        workspaceNames = owners.map(\.workspaceName)
    }
}

struct LocalCredentialRevocationReview: Identifiable {
    let id = UUID()
    let workspaceID: UUID, revision: UUID
    let credentialID: String, algorithm: String, fingerprint: String
    let workspaceNames: [String]
    fileprivate let epoch: UUID
}

/// Existing verified-page saves remain available alongside the current partial
/// decoder cache. Each action keeps the original store's exact review witness.
enum SavedOpticalScan: Identifiable, Sendable {
    case partial(OpticalDecoderRecoveryReview), verified(OpticalScanRecoveryReview)
    var id: UUID { switch self { case .partial(let value): value.id; case .verified(let value): value.id } }
    var identity: OpticalFrame.Identity {
        switch self { case .partial(let value): value.identity; case .verified(let value): value.identity }
    }
    var verifiedPages: Int { switch self { case .partial(let value): value.verifiedPages; case .verified(let value): value.verifiedPages } }
    var verifiedBytes: Int { switch self { case .partial(let value): value.verifiedBytes; case .verified(let value): value.verifiedBytes } }
    var independentParts: Int { switch self { case .partial(let value): value.independentParts; case .verified: 0 } }
}

@MainActor
@Observable
final class MobileWorkspaceModel {
    var workspaces: [InstalledWorkspace] = []
    var availableCredentials: [UUID: Set<String>] = [:]
    var localPolicies: [UUID: WorkspaceLocalState] = [:]
    var locallyRevokedFingerprints: Set<String> = []
    var unconfirmedCredentialRevocations: Set<String> = []
    var credentialMessage: String?
    var credentialImport: WorkspaceKeyImportController?
    @ObservationIgnored private var credentialImportOperation: UUID?
    var state: OfflineRestoreStatus? { didSet { updateIdleTimer(); reconcileScanSaving() } }
    var message: String?
    var showRestore = false { didSet { updateIdleTimer() } }
    var cameraRequested = false { didSet { updateIdleTimer() } }
    var active = false { didSet { updateIdleTimer() } }
    /// Not in the background (see setScenePhase): live connections may run.
    @ObservationIgnored private var connectionsForeground = false
    private var operationBusy = false
    /// Import settlement retains its own reservation across scene transitions
    /// and any older callback that releases the shared operation flag.
    var busy: Bool {
        get { operationBusy || credentialImportOperation != nil || isReadingRecording }
        set { operationBusy = newValue }
    }
    private(set) var isReadingRecording = false
    private(set) var recordingProgress: OpticalRecordingProgress?
    @ObservationIgnored private var recordingRead: (id: UUID, task: Task<Void, Never>)?
    var requestingCameraAccess = false
    var offlineModeBlocked = false
    var savedScans: [SavedOpticalScan] = []
    var scanProgressMessage: String?
    private var scanSaveSession: OpticalDecoderSaveSession?
    private var scanSaveFailure: (data: Data, prior: OpticalDecoderSaveSession, generation: UUID)?
    var isSavingScanProgress: Bool { scanSaveSession != nil && scanSaveFailure == nil }
    var canRetryScanSave: Bool { scanSaveFailure != nil }
    @ObservationIgnored private var scanRecovery: OpticalDecoderRecovery<AppleCryptography>?
    @ObservationIgnored private var verifiedScanRecovery: OpticalScanRecovery<AppleCryptography>?
    @ObservationIgnored private var scanSaveEpoch = UUID()
    @ObservationIgnored private var scanSaveRevision = UUID()
    @ObservationIgnored private var scanRefreshEpoch = UUID()
    @ObservationIgnored private var scanSaveGeneration: UUID?
    @ObservationIgnored private var scanSaveIdentity: OpticalFrame.Identity?
    @ObservationIgnored private var scanCadence: OpticalDecoderSaveCadence?
    @ObservationIgnored private var scanLastSaved = ContinuousClock.now
    @ObservationIgnored private var scanWrite: (id: UUID, task: Task<Void, Never>)?
    @ObservationIgnored private var scanSaveTimer: (id: UUID, task: Task<Void, Never>)?
    @ObservationIgnored private var store: WorkspaceStore?
    /// Live direct-SSH services (TerminalSessionScreen.swift), created with the store.
    @ObservationIgnored private(set) var connections: MobileConnections?
    @ObservationIgnored private var connectionLifecycle: Task<Void, Never>?
    @ObservationIgnored private var restore: OfflineRestore<AppleCryptography>?
    @ObservationIgnored private var bootstrapStarted = false
    @ObservationIgnored private var disconnectReviewID: UUID?
    @ObservationIgnored private var restoreDisconnection: (id: UUID, task: Task<Void, Never>)?
    @ObservationIgnored private var credentialReviewID: UUID?
    @ObservationIgnored private var credentialRevocation: (id: UUID, task: Task<Void, Never>)?
    private var pendingImport: PendingFileImport?
    @ObservationIgnored private var epoch = UUID()
    @ObservationIgnored private var lifecycle: Task<Void, Never>?
    @ObservationIgnored private var workspaceRefresh: Task<Void, Never>?
    @ObservationIgnored private var workspaceRefreshEpoch = UUID()
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
            // The next scan starts over from a finished restore.
            if self.state?.phase == .complete, let restore = self.restore {
                do {
                    let fresh = try await self.restartFinished(restore)
                    guard self.active, self.showRestore, self.epoch == ticket else { return }
                    self.state = fresh; self.lastObservation = nil
                } catch { self.report(error); return }
            }
            guard DataScannerViewController.isSupported else {
                self.message = "This device does not support the native scanner. Open a local encrypted file instead."
                return
            }
            let status = AVCaptureDevice.authorizationStatus(for: .video)
            if status == .authorized {
                if self.observations == nil, let restore = self.restore { self.startObservations(restore: restore) }
                self.cameraRequested = true; return
            }
            guard status == .notDetermined else {
                self.message = "Camera access is denied or restricted. Allow it in Settings or use a local encrypted file."
                return
            }
            let granted = await AVCaptureDevice.requestAccess(for: .video)
            guard self.showRestore else { return }
            // The permission sheet can deactivate the scene. Do not restart a
            // camera in an inactive scene or after the restore sheet was closed.
            if granted, self.active, self.epoch == ticket, !self.offlineModeBlocked,
               self.state?.isSuspended == false {
                if self.observations == nil, let restore = self.restore { self.startObservations(restore: restore) }
                self.cameraRequested = true
            }
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
                // A dedicated protected namespace, separate from workspace and
                // import journals. Opening it does not opt a scan into saving.
                do {
                    verifiedScanRecovery = try OpticalScanRecovery(directory: support.appendingPathComponent("FrankenTermScanRecovery", isDirectory: true),
                                                                   using: AppleCryptography())
                    scanRecovery = try OpticalDecoderRecovery(directory: support.appendingPathComponent("FrankenTermDecoderRecovery", isDirectory: true),
                                                              using: AppleCryptography())
                } catch {
                    scanProgressMessage = "Saving scan progress is unavailable. You can still scan and restore normally. \(error.localizedDescription)"
                }
                connections = try MobileConnections(store: created,
                    journalDirectory: support.appendingPathComponent("FrankenTermSessions", isDirectory: true))
                updateConnectionLifecycle()
            }
            try await refreshWorkspaces(using: created, epoch: epoch)
            if active, pendingImport != nil { showRestore = true }
            restoreLiveScreen()
            updateLifecycle()
            Task { await Self.connectSelfTestIfRequested(using: created) }
        } catch { report(error); bootstrapStarted = false }
    }

    /// `FT_CONNECT_SELF_TEST=1` reaches every saved route up to its host key,
    /// four at a time, without a key or a prompt (WorkspaceRouteProbe), and
    /// writes Documents/connect-self-test.txt.
    private static func connectSelfTestIfRequested(using store: WorkspaceStore) async {
        guard ProcessInfo.processInfo.environment["FT_CONNECT_SELF_TEST"] == "1" else { return }
        var report = ["tailnet address: \(WorkspaceRouteNetwork.deviceTailnetAddress() ?? "none")"]
        do {
            for entry in try await store.loadWithReadiness(support: WorkspaceNativeSSHAuthentication.routeSupport) {
                let document = try entry.workspace.document()
                report.append("workspace \(document.name): keys on device \(entry.credentialIDs.sorted())")
                var lines: [Int: String] = [:]
                for start in stride(from: 0, to: document.routes.count, by: 4) {
                    await withTaskGroup(of: (Int, String).self) { group in
                        for index in start..<min(start + 4, document.routes.count) {
                            let route = document.routes[index]
                            let readiness = entry.routes.first { $0.routeID == route.id }
                            group.addTask {
                                let probe = await WorkspaceRouteProbe.run(route: route, document: document)
                                var line = "\(route.id) \(route.user)@\(route.host):\(route.port) \(probe.network)"
                                line += " ready=\(readiness?.isLocallyReady ?? false) creds=\(readiness?.usableCredentialIDs ?? [])"
                                if let issues = readiness?.issues, !issues.isEmpty { line += " issues=\(issues)" }
                                if let peer = probe.peer {
                                    line += " kex=\(peer.keyExchange) cipher=\(peer.outboundCipher)/\(peer.inboundCipher)"
                                    line += " mac=\(peer.outboundMAC)/\(peer.inboundMAC) hostkey=\(peer.hostSignature)"
                                }
                                if let decision = probe.hostKey { line += " trust=\(decision)" }
                                if let failure = probe.failure {
                                    line += " FAILED \(failure): \((failure as? LocalizedError)?.errorDescription ?? "")"
                                }
                                return (index, line)
                            }
                        }
                        for await (index, line) in group { lines[index] = line }
                    }
                }
                report += lines.keys.sorted().compactMap { lines[$0] }
            }
        } catch { report.append("FAILED: \(error)") }
        guard let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first else { return }
        try? Data((report.joined(separator: "\n") + "\n").utf8)
            .write(to: documents.appendingPathComponent("connect-self-test.txt"), options: .atomic)
    }
    /// Inactive (a Face ID sheet, a permission alert, Control Center, the app
    /// switcher) covers the screen and pauses restore. Live sessions run until
    /// the user closes them, background included: leaving asks iOS for its
    /// usual grace period and ends the standing Face ID approval; returning
    /// reconnects whatever dropped meanwhile (WorkspaceTerminalScene).
    func setScenePhase(_ phase: ScenePhase) {
        if phase == .background {
            AppleSSHCredentialSigner.endApproval()
            beginBackgroundRun()
        } else { endBackgroundRun() }
        let foreground = phase != .background
        if foreground != connectionsForeground {
            connectionsForeground = foreground
            if foreground { updateConnectionLifecycle() }
        }
        // Scenes run whether or not their screen is showing, so they hear the
        // app's phase from here, not from their screen.
        for scene in liveScenes.values {
            scene.setForeground(phase == .active)
            if phase == .background { scene.enteredBackground() }
        }
        if (phase == .active) != active { setActive(phase == .active) }
    }
    @ObservationIgnored private var backgroundRun = UIBackgroundTaskIdentifier.invalid
    /// Keeps live sessions answering their servers through a quick app switch.
    private func beginBackgroundRun() {
        guard backgroundRun == .invalid else { return }
        backgroundRun = UIApplication.shared.beginBackgroundTask(withName: "FrankenTerm sessions") { [weak self] in
            MainActor.assumeIsolated { self?.endBackgroundRun() }
        }
    }
    private func endBackgroundRun() {
        guard backgroundRun != .invalid else { return }
        UIApplication.shared.endBackgroundTask(backgroundRun)
        backgroundRun = .invalid
    }

    /// Live workspace scenes outlive their screens: leaving the terminal keeps
    /// every session running until the user disconnects it. A new revision of
    /// a workspace (a re-import) or its removal retires its scene.
    @ObservationIgnored private var liveScenes: [UUID: WorkspaceTerminalScene] = [:]
    func liveScene(workspaceID: UUID, revision: UUID,
                   make: () throws -> WorkspaceTerminalScene) throws -> WorkspaceTerminalScene {
        guard credentialRevocation == nil, credentialImportOperation == nil else { throw WorkspaceSSHConnectionError.busy }
        if let saved = workspaces.first(where: { $0.id == workspaceID }), let document = try? saved.document(),
           document.credentials.contains(where: { unconfirmedCredentialRevocations.contains($0.fingerprint) }) {
            throw SSHAuthenticationError.denied
        }
        if let scene = liveScenes[workspaceID], scene.revision == revision, !scene.closed { return scene }
        if let stale = liveScenes.removeValue(forKey: workspaceID) { retire(stale) }
        let scene = try make()
        liveScenes[workspaceID] = scene
        return scene
    }
    /// What a machine's live session is doing, once its workspace has a scene.
    func liveStatus(_ workspaceID: UUID, machineID: String) -> WorkspaceTerminalScene.Status? {
        liveScenes[workspaceID].flatMap { $0.closed ? nil : $0.status(of: machineID) }
    }
    private func retire(_ scene: WorkspaceTerminalScene) {
        scene.requestClose()
        Task { await scene.close() }
    }
    private func setActive(_ value: Bool) {
        active = value
        if !value {
            credentialImport?.cancel()
            scannedCode = nil; stopSavingScanProgress()
            cameraRequested = false; state = nil; availableCredentials = [:]; localPolicies = [:]
            locallyRevokedFingerprints = []
            // A system permission sheet can interrupt a read. Do not turn its
            // foreground callback into another automatic prompt/read attempt.
            if busy { pendingImport?.needsExplicitRetry = true }
        } else if pendingImport != nil { showRestore = true }
        updateLifecycle()
    }
    /// Serialized activations: admit connections and retry failed cleanup.
    private func updateConnectionLifecycle() {
        guard let connections else { return }
        let prior = connectionLifecycle
        connectionLifecycle = Task {
            await prior?.value
            await connections.activate()
        }
    }
    func setRestoreVisible(_ value: Bool) {
        if value { credentialImport?.cancel() }
        showRestore = value
        // A just-dismissed request may already be retired. Re-present only the
        // status returned by the serialized lifecycle transition.
        state = nil
        if !value {
            cameraRequested = false; pendingImport = nil; offlineModeBlocked = false
            scannedCode = nil; stopSavingScanProgress()
        }
        updateLifecycle(discardRecipientRequest: !value)
    }
    func retryOfflineMode() {
        guard active, showRestore, !busy else { return }
        message = nil
        updateLifecycle()
    }

    private var disconnectableScenes: [WorkspaceTerminalScene] {
        liveScenes.values.filter { scene in
            !scene.closed || scene.sessions.values.contains { !$0.settled }
        }.sorted {
            let order = $0.workspaceName.localizedStandardCompare($1.workspaceName)
            return order == .orderedSame ? $0.workspaceID.uuidString < $1.workspaceID.uuidString
                : order == .orderedAscending
        }
    }

    var canReviewSessionsForRestore: Bool {
        active && showRestore && offlineModeBlocked && !busy && !disconnectableScenes.isEmpty
    }

    func reviewSessionsForRestore() -> RestoreSessionDisconnectReview? {
        guard active, showRestore, offlineModeBlocked, !busy, restoreDisconnection == nil else { return nil }
        let owners = disconnectableScenes
        guard !owners.isEmpty else { return nil }
        let reviewed = RestoreSessionDisconnectReview(epoch: epoch, owners: owners)
        disconnectReviewID = reviewed.id
        return reviewed
    }

    /// Validate the complete review before sealing any owner. New, unreviewed
    /// scenes keep their sessions and can still prevent offline admission.
    func disconnectSessionsAndRestore(_ reviewed: RestoreSessionDisconnectReview) {
        guard active, showRestore, offlineModeBlocked, !busy, restoreDisconnection == nil else { return }
        guard disconnectReviewID == reviewed.id, reviewed.epoch == epoch, !reviewed.owners.isEmpty,
              reviewed.owners.allSatisfy({ liveScenes[$0.workspaceID] === $0 }) else {
            message = "The workspace sessions changed. Review them again before disconnecting; this confirmation did not close any sessions."
            return
        }
        disconnectReviewID = nil
        busy = true; message = nil; cameraRequested = false
        // No actor hop may intervene between validating the owners and sealing
        // input/reconnect admission for every one of them.
        for owner in reviewed.owners { owner.requestClose() }
        let job = Task { [weak self] in
            // The model retains this task even after leaving restore. Closing
            // joins each owner's timers, startup, input and transport cleanup;
            // MobileConnections keeps any failed settlements for a later retry.
            for owner in reviewed.owners { await owner.close() }
            guard let self, self.restoreDisconnection?.id == reviewed.id else { return }
            self.restoreDisconnection = nil; self.busy = false
            guard self.active, self.showRestore, self.epoch == reviewed.epoch else { return }
            self.updateLifecycle()
        }
        restoreDisconnection = (reviewed.id, job)
    }

    func requestRecipient() {
        guard active, !busy else { return }
        scannedCode = nil; stopSavingScanProgress()
        pendingImport = nil; cameraRequested = false; setRestoreVisible(true)
        run { [self] restore, state in
            let current = state.phase == .complete ? try await self.restartFinished(restore) : state
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
        scannedCode = nil; stopSavingScanProgress()
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
        disconnectReviewID = nil
        credentialReviewID = nil
        workspaceRefreshEpoch = UUID()
        workspaceRefresh?.cancel()
        observations?.finish(); observations = nil
        observationTask?.cancel(); observationTask = nil; lastObservation = nil
        let pendingRecording = recordingRead?.task
        pendingRecording?.cancel()
        guard let restore else { return }
        let ticket = epoch, visible = active && showRestore, prior = lifecycle
        lifecycle = Task { [weak self] in
            await prior?.value
            // The exact reader retires before a lifecycle transition rotates
            // receive authority. A background callback cannot start more work.
            await pendingRecording?.value
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
            if self.active { await self.refreshSavedScan(epoch: ticket) }
        }
    }

    private func cancelScanSaveTimer() {
        scanSaveTimer?.task.cancel(); scanSaveTimer = nil
    }

    /// Retire local admission synchronously, then join the retained writer and
    /// revoke its storage epoch. A delayed write cannot recreate retired rows.
    func stopSavingScanProgress() {
        cancelScanSaveTimer()
        let ended = scanSaveSession ?? scanSaveFailure?.prior, prior = scanWrite
        scanSaveEpoch = UUID(); scanSaveSession = nil; scanSaveFailure = nil
        scanSaveGeneration = nil; scanSaveIdentity = nil; scanCadence = nil
        guard let ended, let scanRecovery else { return }
        let id = UUID()
        let job = Task { [weak self] in
            await prior?.task.value
            do { try await scanRecovery.stopSaving(ended) }
            catch OpticalDecoderRecoveryError.staleWriter { } // already retired
            catch {
                self?.scanProgressMessage = "Saving has stopped in this app, but retiring the saved writer could not be confirmed. Review saved progress before saving again. \(error.localizedDescription)"
            }
            guard let self, self.scanWrite?.id == id else { return }
            self.scanWrite = nil
            if self.active { await self.refreshSavedScan(epoch: self.epoch) }
        }
        scanWrite = (id, job)
    }

    func continueWithoutSaving() {
        stopSavingScanProgress()
        scanProgressMessage = "Scanning can continue. Previously saved progress is still available."
        advanceAutomatically()
    }

    private func reconcileScanSaving() {
        guard scanSaveSession != nil else { return }
        guard let state, !state.isSuspended, state.source == .optical,
              state.identity == scanSaveIdentity, state.phase != .complete else {
            stopSavingScanProgress(); return
        }
        if scanSaveGeneration != state.generation {
            cancelScanSaveTimer()
            // A failed snapshot belongs to its old decoder generation. It may
            // not become a retry after a page repair or a source transition.
            if scanSaveFailure != nil { stopSavingScanProgress() }
            else { scanSaveGeneration = state.generation }
        }
    }

    private func publishSavedScan(_ reviewed: OpticalDecoderRecoveryReview) {
        scanRefreshEpoch = UUID()
        savedScans.removeAll { if case .partial = $0 { true } else { false } }
        savedScans.insert(.partial(reviewed), at: 0)
    }

    private func refreshSavedScan(epoch ticket: UUID) async {
        let refresh = UUID(); scanRefreshEpoch = refresh
        var found: [SavedOpticalScan] = [], failures: [String] = []
        if let scanRecovery {
            do { if let value = try await scanRecovery.review() { found.append(.partial(value)) } }
            catch is CancellationError { return }
            catch { failures.append(error.localizedDescription) }
        }
        if let verifiedScanRecovery {
            do { if let value = try await verifiedScanRecovery.review() { found.append(.verified(value)) } }
            catch is CancellationError { return }
            catch { failures.append(error.localizedDescription) }
        }
        guard active, epoch == ticket, scanRefreshEpoch == refresh else { return }
        savedScans = found
        if !failures.isEmpty { scanProgressMessage = "Some saved progress could not be read. " + failures.joined(separator: " ") }
    }

    /// User actions join the existing writer. Initial consent failures can have
    /// published, so read the actual store and require a new exact review.
    private func scanOperation(advanceWhenFinished: Bool = true, _ operation: @escaping @MainActor
        (OfflineRestore<AppleCryptography>, OpticalDecoderRecovery<AppleCryptography>, UUID) async throws -> Void) {
        guard active, showRestore, !busy, !offlineModeBlocked,
              let restore, let scanRecovery else { return }
        busy = true; scanProgressMessage = nil; cancelScanSaveTimer()
        let ticket = epoch, pendingLifecycle = lifecycle
        Task { [weak self] in
            await pendingLifecycle?.value
            guard let self else { return }
            await self.scanWrite?.task.value
            defer {
                self.busy = false
                if advanceWhenFinished { self.advanceAutomatically() }
            }
            guard self.active, self.showRestore, self.epoch == ticket, !self.offlineModeBlocked else { return }
            do { try await operation(restore, scanRecovery, ticket) }
            catch is CancellationError { }
            catch {
                guard self.active, self.epoch == ticket else { return }
                self.stopSavingScanProgress()
                self.scanProgressMessage = "Saving was not confirmed. Review the saved progress before trying again. \(error.localizedDescription)"
                let latest = await restore.status()
                guard self.active, self.epoch == ticket else { return }
                self.state = latest
            }
            if self.active, self.epoch == ticket { await self.refreshSavedScan(epoch: ticket) }
        }
    }

    private func adoptScanWriter(_ session: OpticalDecoderSaveSession, generation: UUID) {
        scanSaveEpoch = UUID(); scanSaveRevision = UUID(); scanSaveSession = session; scanSaveFailure = nil
        scanSaveGeneration = generation; scanSaveIdentity = session.review.identity
        scanCadence = .init(receivedParts: session.review.receivedParts, verifiedPages: session.review.verifiedPages)
        scanLastSaved = .now; publishSavedScan(session.review)
    }

    func saveScanProgress(generation: UUID, identity: OpticalFrame.Identity,
                          replacing reviewed: OpticalDecoderRecoveryReview?) {
        scanOperation { [weak self] restore, recovery, ticket in
            guard let self, self.state?.generation == generation, self.state?.identity == identity else { throw FTError.stale }
            let checkpoint = try await restore.exportDecoderCheckpoint(generation: generation)
            guard self.active, self.epoch == ticket else { throw CancellationError() }
            let writer = self.scanSaveEpoch
            let session: OpticalDecoderSaveSession
            if let reviewed {
                guard reviewed.identity == identity else { throw FTError.stale }
                session = try await recovery.continueSaving(reviewed, confirmedByUser: true)
            } else { session = try await recovery.beginSaving(checkpoint, confirmedByUser: true) }
            guard self.active, self.epoch == ticket, self.scanSaveEpoch == writer else {
                try? await recovery.stopSaving(session); return
            }
            self.adoptScanWriter(session, generation: generation)
            let latest = await restore.status()
            guard self.active, self.epoch == ticket else { return }
            self.state = latest
            await self.saveScanSnapshot(latest, restore: restore, epoch: ticket, force: true)
        }
    }

    func resumeSavedScan(_ reviewed: SavedOpticalScan) {
        scanOperation { [weak self] restore, recovery, ticket in
            guard let self else { return }
            self.scannedCode = nil; self.stopSavingScanProgress(); self.cameraRequested = false
            await self.scanWrite?.task.value
            let current = await restore.status()
            let ready = current.phase == .complete ? try await self.restartFinished(restore) : current
            guard ready.phase == .idle else { throw WorkspaceStoreError.busy }
            let resumed: OfflineRestoreStatus, session: OpticalDecoderSaveSession
            let writer = self.scanSaveEpoch
            switch reviewed {
            case .partial(let saved):
                let checkpoint = try await recovery.load(saved)
                guard self.active, self.epoch == ticket else { throw CancellationError() }
                resumed = try await restore.acceptDecoderCheckpoint(checkpoint, generation: ready.generation)
                guard self.active, self.epoch == ticket else { return }
                session = try await recovery.continueSaving(saved, confirmedByUser: true)
            case .verified(let saved):
                guard let legacy = self.verifiedScanRecovery else { throw FTError.storage }
                guard try await recovery.review() == nil else { throw OpticalDecoderRecoveryError.savedProgressExists }
                let checkpoint = try await legacy.load(saved)
                guard self.active, self.epoch == ticket else { throw CancellationError() }
                resumed = try await restore.resumeScan(checkpoint, generation: ready.generation)
                let decoder = try await restore.exportDecoderCheckpoint(generation: resumed.generation)
                guard self.active, self.epoch == ticket else { return }
                session = try await recovery.beginSaving(decoder, confirmedByUser: true)
            }
            guard self.active, self.epoch == ticket, self.scanSaveEpoch == writer else {
                try? await recovery.stopSaving(session); return
            }
            self.state = resumed; self.lastObservation = nil
            self.adoptScanWriter(session, generation: resumed.generation)
        }
    }

    func discardSavedScan(_ reviewed: SavedOpticalScan) {
        scanOperation { [weak self] _, recovery, _ in
            guard let self else { return }
            // Discard itself rotates the epoch using the exact reviewed bytes;
            // rotating it with stopSaving first would invalidate this review.
            switch reviewed {
            case .partial(let saved): try await recovery.discard(saved, confirmedByUser: true)
            case .verified(let saved):
                guard let legacy = self.verifiedScanRecovery else { throw FTError.storage }
                try await legacy.discard(saved)
            }
            self.stopSavingScanProgress()
        }
    }

    /// Drain the bounded camera stream before the final snapshot. No suspend or
    /// source mutation can overtake this explicitly requested save.
    func saveAndPauseScan() {
        scanOperation(advanceWhenFinished: false) { [weak self] restore, _, ticket in
            guard let self else { return }
            self.cameraRequested = false; self.scannedCode = nil
            let consumer = self.observationTask
            self.observations?.finish(); self.observations = nil; self.observationTask = nil
            await consumer?.value
            guard self.active, self.epoch == ticket else { return }
            let current = await restore.status()
            let priorRevision = self.scanSaveRevision
            await self.saveScanSnapshot(current, restore: restore, epoch: ticket, force: true)
            guard self.active, self.epoch == ticket, self.scanSaveFailure == nil,
                  self.scanSaveSession != nil, self.scanSaveRevision != priorRevision else { return }
            // Announce only a confirmed write. A later retirement/read failure
            // can replace this message and must not be hidden by success text.
            self.scanProgressMessage = "Scan progress saved. Resume the camera when you are ready."
            self.stopSavingScanProgress()
            await self.scanWrite?.task.value
        }
    }

    func retryScanSave() {
        scanOperation { [weak self] restore, _, ticket in
            guard let self, let failed = self.scanSaveFailure,
                  self.scanSaveGeneration == failed.generation,
                  self.state?.generation == failed.generation else { throw FTError.stale }
            await self.writeScanSnapshot(failed.data, after: failed.prior, generation: failed.generation, epoch: ticket)
            guard self.active, self.epoch == ticket, self.scanSaveFailure == nil else { return }
            let latest = await restore.status()
            await self.saveScanSnapshot(latest, restore: restore, epoch: ticket, force: true)
        }
    }

    /// One timer coalesces useful frame progress, including a trailing partial
    /// burst. Complete envelopes and deliberate repairs bypass the interval.
    private func saveScanSnapshot(_ status: OfflineRestoreStatus, restore: OfflineRestore<AppleCryptography>,
                                  epoch ticket: UUID, force: Bool = false) async {
        guard !Task.isCancelled, active, showRestore, epoch == ticket, (!busy || isReadingRecording || force),
              scanSaveFailure == nil, scanSaveSession != nil else { return }
        let writer = scanSaveEpoch
        var current = status
        // A repair or completion must follow any earlier snapshot, even when
        // that owner is still exporting its bytes rather than writing them.
        while let pending = scanWrite {
            guard force || current.phase == .locked || current.phase == .review else { return }
            cancelScanSaveTimer()
            await pending.task.value
            guard !Task.isCancelled, active, showRestore, epoch == ticket,
                  scanSaveEpoch == writer, scanSaveFailure == nil else { return }
            current = await restore.status()
        }
        guard !Task.isCancelled, active, showRestore, epoch == ticket, scanSaveEpoch == writer,
              (!busy || isReadingRecording || force), scanSaveFailure == nil, let prior = scanSaveSession, let cadence = scanCadence,
              scanSaveGeneration == current.generation, scanSaveIdentity == current.identity,
              [.collecting, .locked, .review].contains(current.phase) else { return }
        guard let ordinaryDelay = cadence.delay(receivedParts: current.receivedParts, verifiedPages: current.verifiedPages,
                                                elapsed: scanLastSaved.duration(to: .now), force: force) else { return }
        // Repeated queued observations of an already-saved locked envelope do
        // not write again. A newly completed envelope does not wait two seconds.
        let delay = current.phase == .collecting ? ordinaryDelay : .zero
        if delay > .zero {
            scheduleScanSaveTimer(after: delay, restore: restore, epoch: ticket,
                                  writer: writer, generation: current.generation)
            return
        }
        cancelScanSaveTimer()
        let id = UUID(), predecessor = scanSaveRevision
        let generation = current.generation, identity = current.identity
        let job = Task { [weak self] in
            guard let self else { return }
            defer { if self.scanWrite?.id == id { self.scanWrite = nil } }
            do {
                let checkpoint = try await restore.exportDecoderCheckpoint(generation: generation)
                guard !Task.isCancelled, self.active, self.showRestore, self.epoch == ticket,
                      self.scanWrite?.id == id, self.scanSaveEpoch == writer,
                      self.scanSaveGeneration == generation, self.scanSaveIdentity == identity,
                      self.scanSaveRevision == predecessor, self.scanSaveFailure == nil else { return }
                await self.writeScanSnapshot(checkpoint, after: prior, generation: generation, epoch: ticket, ownedBy: id)
            } catch FTError.stale {
                // A manual repair may rotate the receiver while this export
                // waits. Its fresh forced snapshot owns the new generation.
            } catch is CancellationError { }
            catch {
                guard !Task.isCancelled, self.active, self.epoch == ticket, self.scanSaveEpoch == writer,
                      self.scanSaveGeneration == generation, self.scanSaveIdentity == identity,
                      self.scanSaveRevision == predecessor else { return }
                self.stopSavingScanProgress()
                self.scanProgressMessage = "Saving paused before a checkpoint could be made. \(error.localizedDescription)"
            }
        }
        // Reserve before the first actor await: another observation or timer
        // cannot export against this same predecessor while it is in flight.
        scanWrite = (id, job)
        await job.value
        guard !Task.isCancelled, active, showRestore, epoch == ticket, scanSaveEpoch == writer,
              scanSaveRevision != predecessor, scanSaveFailure == nil, scanWrite == nil else { return }
        let latest = await restore.status()
        guard !Task.isCancelled, active, showRestore, epoch == ticket, scanSaveEpoch == writer,
              scanSaveGeneration == latest.generation, scanSaveIdentity == latest.identity,
              scanSaveFailure == nil, scanWrite == nil, let latestCadence = scanCadence,
              [.collecting, .locked, .review].contains(latest.phase),
              let trailingDelay = latestCadence.delay(receivedParts: latest.receivedParts,
                  verifiedPages: latest.verifiedPages, elapsed: scanLastSaved.duration(to: .now)) else { return }
        if latest.phase == .collecting {
            // Frames can arrive during file I/O. Retain their final partial
            // burst even if no further camera callback arrives to start a timer.
            scheduleScanSaveTimer(after: trailingDelay, restore: restore, epoch: ticket,
                                  writer: writer, generation: latest.generation)
        } else {
            // Completion must reach storage before this caller advances to
            // decryption/import. The changed-count guard above bounds this path.
            await saveScanSnapshot(latest, restore: restore, epoch: ticket, force: true)
        }
    }

    private func scheduleScanSaveTimer(after delay: Duration, restore: OfflineRestore<AppleCryptography>,
                                       epoch ticket: UUID, writer: UUID, generation: UUID) {
        guard scanSaveTimer == nil, active, showRestore, epoch == ticket,
              scanSaveEpoch == writer, scanSaveGeneration == generation, scanSaveFailure == nil else { return }
        let id = UUID()
        let job = Task { [weak self] in
            do { try await Task.sleep(for: delay) } catch { return }
            guard let self else { return }
            // A manual action may be finishing when the deadline arrives.
            // Keep one cancellable timer instead of losing its trailing save.
            while self.busy && !self.isReadingRecording {
                guard self.scanSaveTimer?.id == id, self.active, self.showRestore,
                      self.epoch == ticket, self.scanSaveEpoch == writer,
                      self.scanSaveGeneration == generation else { return }
                do { try await Task.sleep(for: .milliseconds(200)) } catch { return }
            }
            guard !Task.isCancelled, self.scanSaveTimer?.id == id, self.active, self.showRestore,
                  self.epoch == ticket, self.scanSaveEpoch == writer,
                  self.scanSaveGeneration == generation, self.scanSaveFailure == nil else { return }
            let latest = await restore.status()
            guard !Task.isCancelled, self.scanSaveTimer?.id == id, self.active, self.showRestore,
                  self.epoch == ticket, self.scanSaveEpoch == writer,
                  self.scanSaveGeneration == generation else { return }
            self.scanSaveTimer = nil
            await self.saveScanSnapshot(latest, restore: restore, epoch: ticket)
        }
        scanSaveTimer = (id, job)
    }

    /// Retain the exact failed payload and predecessor. New frames never replace
    /// an uncertain write; Retry must reaffirm that same successor first.
    private func writeScanSnapshot(_ checkpoint: Data, after prior: OpticalDecoderSaveSession,
                                   generation: UUID, epoch ticket: UUID, ownedBy owner: UUID? = nil) async {
        guard !Task.isCancelled, active, showRestore, epoch == ticket,
              scanSaveGeneration == generation, scanSaveIdentity == prior.review.identity,
              let recovery = scanRecovery else { return }
        if owner == nil {
            guard scanWrite == nil else { return }
            let id = UUID()
            let job = Task { [weak self] in
                guard let self else { return }
                defer { if self.scanWrite?.id == id { self.scanWrite = nil } }
                await self.writeScanSnapshot(checkpoint, after: prior, generation: generation, epoch: ticket, ownedBy: id)
            }
            scanWrite = (id, job)
            await job.value
            return
        }
        guard scanWrite?.id == owner else { return }
        let writer = scanSaveEpoch
        do {
            let saved = try await recovery.save(checkpoint, after: prior)
            guard active, epoch == ticket, scanSaveEpoch == writer else { return }
            scanSaveRevision = UUID(); scanSaveSession = saved; scanSaveFailure = nil
            scanCadence?.record(receivedParts: saved.review.receivedParts, verifiedPages: saved.review.verifiedPages)
            scanLastSaved = .now; publishSavedScan(saved.review)
        } catch {
            guard active, epoch == ticket, scanSaveEpoch == writer else { return }
            guard scanSaveGeneration == generation else {
                stopSavingScanProgress()
                scanProgressMessage = "Saving could not be confirmed before this scan changed. Review saved progress before saving again."
                return
            }
            scanSaveFailure = (checkpoint, prior, generation)
            cancelScanSaveTimer()
            scanProgressMessage = "Saving could not be confirmed. Retry Saving to preserve this exact checkpoint, or continue without saving. \(error.localizedDescription)"
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
        guard !Task.isCancelled, active, epoch == ticket else { return }
        let request = UUID(); workspaceRefreshEpoch = request
        let snapshot: [WorkspaceLocalState], revoked: Set<String>
        do {
            snapshot = try await store.loadWithLocalPolicy(support: WorkspaceNativeSSHAuthentication.routeSupport)
            revoked = try await store.locallyRevokedCredentialFingerprints()
        } catch {
            guard !Task.isCancelled, active, epoch == ticket, workspaceRefreshEpoch == request else { return }
            throw error
        }
        guard !Task.isCancelled, active, epoch == ticket, workspaceRefreshEpoch == request else { return }
        workspaces = snapshot.map(\.workspace)
        for (id, scene) in liveScenes where !workspaces.contains(where: { $0.id == id && $0.revision == scene.revision }) {
            liveScenes[id] = nil; retire(scene)
        }
        availableCredentials = Dictionary(uniqueKeysWithValues: snapshot.map { ($0.workspace.id, $0.credentialIDs) })
        localPolicies = Dictionary(uniqueKeysWithValues: snapshot.map { ($0.workspace.id, $0) })
        locallyRevokedFingerprints = revoked
        if let importing = credentialImport, !importing.isCommitting, !importing.isFinished,
           !credentialImportIsCurrent(importing) {
            importing.cancel(message: "The workspace or key changed. Select its current key reference again.")
        }
        // A first import, or new machines in a replaced one: machines that
        // look alike get distinct themes before anything shows them.
        for saved in workspaces {
            if let document = try? saved.document(), let themes = machineThemes(saved.id, document: document) {
                setThemeChoices(themes.distinguishingMachines(), for: saved.id)
            }
        }
    }

    func beginCredentialImport(workspaceID: UUID, credentialID: String) {
        guard active, !busy, !showRestore, credentialImport == nil, let store,
              let saved = workspaces.first(where: { $0.id == workspaceID }),
              let document = try? saved.document(),
              let reference = document.credentials.first(where: { $0.id == credentialID }),
              !locallyRevokedFingerprints.contains(reference.fingerprint),
              !unconfirmedCredentialRevocations.contains(reference.fingerprint) else { return }
        credentialReviewID = nil; credentialMessage = nil
        credentialImport = WorkspaceKeyImportController(model: self, store: store,
            workspaceID: saved.id, revision: saved.revision, credential: reference, epoch: epoch)
    }

    /// Every Files/passphrase callback belongs to this exact sheet, foreground
    /// epoch and accepted identity. Finished receipts can show their own result
    /// after the transaction has changed the saved revision.
    fileprivate func credentialImportIsCurrent(_ importing: WorkspaceKeyImportController,
                                               allowChangedRevision: Bool = false) -> Bool {
        guard credentialImport === importing, active, !showRestore, epoch == importing.epoch,
              !locallyRevokedFingerprints.contains(importing.credential.fingerprint),
              !unconfirmedCredentialRevocations.contains(importing.credential.fingerprint),
              let saved = workspaces.first(where: { $0.id == importing.workspaceID }),
              let document = try? saved.document(), document.credentials.contains(where: {
            $0.id == importing.credential.id && $0.algorithm == importing.credential.algorithm
                && $0.fingerprint == importing.credential.fingerprint
              }) else { return false }
        return allowChangedRevision || saved.revision == importing.revision
    }

    /// Final confirmation discloses this workspace's session shutdown. Seal the
    /// exact scene before awaiting cleanup; no replacement can open meanwhile.
    fileprivate func prepareCredentialImport(_ importing: WorkspaceKeyImportController) async throws {
        guard credentialImportIsCurrent(importing), !busy, credentialImportOperation == nil else {
            throw WorkspaceStoreError.staleReview
        }
        credentialImportOperation = importing.id; busy = true; credentialReviewID = nil
        workspaceRefreshEpoch = UUID(); workspaceRefresh?.cancel()
        let owner = liveScenes[importing.workspaceID]
        owner?.requestClose()
        if let owner {
            await owner.close()
            if liveScenes[importing.workspaceID] === owner { liveScenes[importing.workspaceID] = nil }
        }
        await connections?.manager.invalidate(workspaceID: importing.workspaceID)
        try Task.checkCancellation()
        guard credentialImportIsCurrent(importing), credentialImportOperation == importing.id else {
            throw WorkspaceStoreError.staleReview
        }
    }

    /// Runs in a retained, uncancelled settlement task. Even a cancelled caller
    /// must reconcile a write which may already have published. Inactive scenes
    /// keep readiness cleared until the normal foreground refresh can run.
    fileprivate func finishCredentialImport(_ importing: WorkspaceKeyImportController,
        result: Result<WorkspaceCredentialImportReceipt, Error>) async -> String {
        guard credentialImportOperation == importing.id else {
            return "The key import stopped before it could begin."
        }
        defer { credentialImportOperation = nil; busy = false }
        workspaceRefreshEpoch = UUID(); workspaceRefresh?.cancel()
        availableCredentials.removeValue(forKey: importing.workspaceID)
        localPolicies.removeValue(forKey: importing.workspaceID)
        var refreshed = false
        if active, let store {
            do {
                try await refreshWorkspaces(using: store, epoch: epoch)
                refreshed = active && localPolicies[importing.workspaceID] != nil
            } catch { }
        }
        let text: String
        switch result {
        case .success(let receipt):
            if !refreshed {
                text = "The key was imported. Reopen SSH Keys when the app is active to refresh connection readiness."
            } else if receipt.cleanupPending {
                text = "The key was imported. Protected-storage cleanup was retried while refreshing its device status. Connect when you are ready."
            } else {
                text = "The matching key is available on this phone. Connect when you are ready."
            }
        case .failure(let error):
            if refreshed {
                let present = availableCredentials[importing.workspaceID]?.contains(importing.credential.id) == true
                let outcome = present ? "The key is currently present on this phone." : "The key is not currently available on this phone."
                text = (error is CancellationError ? "Key import stopped. " : "The import reported a problem. "
                    + WorkspaceKeyImportController.describe(error) + " ") + outcome
            } else {
                text = "The key import outcome could not be refreshed. Reopen SSH Keys when the app is active to check its actual device status before trying again."
            }
        }
        credentialMessage = text
        return text
    }

    fileprivate func releaseCredentialImport(_ importing: WorkspaceKeyImportController) {
        if credentialImport === importing { credentialImport = nil }
    }

    func reviewCredentialRevocation(workspaceID: UUID, credentialID: String) -> LocalCredentialRevocationReview? {
        guard active, !busy, !showRestore, credentialImportOperation == nil, store != nil, connections != nil,
              let saved = workspaces.first(where: { $0.id == workspaceID }),
              let document = try? saved.document(),
              let key = document.credentials.first(where: { $0.id == credentialID }),
              !locallyRevokedFingerprints.contains(key.fingerprint)
                || unconfirmedCredentialRevocations.contains(key.fingerprint) else { return nil }
        let names = workspaces.compactMap { saved -> String? in
            guard let document = try? saved.document(),
                  document.credentials.contains(where: { $0.fingerprint == key.fingerprint }) else { return nil }
            return document.name
        }.sorted { $0.localizedStandardCompare($1) == .orderedAscending }
        let reviewed = LocalCredentialRevocationReview(workspaceID: workspaceID, revision: saved.revision,
            credentialID: key.id, algorithm: key.algorithm, fingerprint: key.fingerprint,
            workspaceNames: names, epoch: epoch)
        credentialReviewID = reviewed.id
        return reviewed
    }

    /// The fingerprint is device-wide, including aliases in other workspaces.
    /// Once admitted, this task owns cleanup through suspension or sheet closure.
    func revokeCredentialLocally(_ reviewed: LocalCredentialRevocationReview) {
        guard active, !busy, !showRestore, credentialImportOperation == nil,
              credentialRevocation == nil, let store, let connections else { return }
        guard credentialReviewID == reviewed.id, epoch == reviewed.epoch,
              let saved = workspaces.first(where: { $0.id == reviewed.workspaceID }), saved.revision == reviewed.revision,
              let document = try? saved.document(), document.credentials.contains(where: {
                  $0.id == reviewed.credentialID && $0.fingerprint == reviewed.fingerprint && $0.algorithm == reviewed.algorithm
              }) else {
            credentialMessage = "This key or workspace changed. Review the key again before revoking it."
            return
        }
        credentialReviewID = nil; credentialMessage = nil
        busy = true; workspaceRefreshEpoch = UUID(); workspaceRefresh?.cancel()
        // Seal native attempt loops before the first actor hop. A refusal
        // already returned by the manager must not authorize the next key
        // while this confirmed revocation is waiting to persist.
        let affectedBeforeWrite = Set(workspaces.compactMap { saved -> UUID? in
            guard let document = try? saved.document(),
                  document.credentials.contains(where: { $0.fingerprint == reviewed.fingerprint }) else { return nil }
            return saved.id
        })
        let sealedOwners = liveScenes.values.filter { affectedBeforeWrite.contains($0.workspaceID) }
        for owner in sealedOwners { owner.requestClose() }
        // The admitted operation retains its owner until exact scene cleanup
        // finishes, including when the review sheet or app scene disappears.
        let job = Task { [self] in
            defer {
                if self.credentialRevocation?.id == reviewed.id {
                    self.credentialRevocation = nil; self.busy = false
                }
            }
            do {
                // The manager persists denial before retiring grants and native
                // transports, and retains failed settlement owners for retry.
                let receipt = try await connections.manager.revokeCredentialLocally(
                    fingerprint: reviewed.fingerprint, confirmedByUser: true)
                // Retire any read that overlapped the write. Reflect confirmed
                // denial immediately, even if the following readback fails.
                self.workspaceRefreshEpoch = UUID(); self.workspaceRefresh?.cancel()
                for id in receipt.affectedWorkspaceIDs {
                    self.availableCredentials.removeValue(forKey: id)
                    self.localPolicies.removeValue(forKey: id)
                }
                if receipt.durableDenialConfirmed {
                    self.locallyRevokedFingerprints.insert(receipt.fingerprint)
                    self.unconfirmedCredentialRevocations.remove(receipt.fingerprint)
                }
                else { self.unconfirmedCredentialRevocations.insert(receipt.fingerprint) }
                var owners = sealedOwners
                for owner in self.liveScenes.values where receipt.affectedWorkspaceIDs.contains(owner.workspaceID)
                    && !owners.contains(where: { $0 === owner }) { owners.append(owner) }
                for owner in owners { owner.requestClose() }
                for owner in owners { await owner.close() }
                let retained = await connections.manager.connections()
                let cleanupPending = owners.contains { $0.sessions.values.contains { !$0.settled } }
                    || retained.contains { receipt.retainedConnectionIDs.contains($0.id) }
                if receipt.durableDenialConfirmed {
                    self.credentialMessage = cleanupPending
                        ? "This key is revoked on this phone. Affected sessions are stopped; local cleanup remains pending and will be retried when the app becomes active."
                        : "This key is revoked on this phone, and affected workspace sessions have been disconnected. Future imports cannot restore its permission."
                } else {
                    self.credentialMessage = "This key is blocked while the app runs, but saving the revocation could not be confirmed. Choose Retry Saving Revocation before closing the app. Affected workspace sessions have been stopped."
                }
                if self.active {
                    do { try await self.refreshWorkspaces(using: store, epoch: self.epoch) }
                    catch { self.credentialMessage = (self.credentialMessage ?? "") + " Device status could not be refreshed: \(error.localizedDescription)" }
                }
            } catch {
                for owner in sealedOwners { await owner.close() }
                self.credentialMessage = "Revocation could not be started. \(error.localizedDescription)"
            }
        }
        credentialRevocation = (reviewed.id, job)
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

    /// A live terminal on screen: its workspace and machine.
    struct LiveScreen: Codable, Hashable {
        let workspaceID: UUID, machineID: String
    }
    private static let liveScreenKey = "live-screen"
    /// Where this launch returns: the live terminal the last one showed. The
    /// root opens it once, and popping it clears this (plan 2.1, return later).
    var returning: LiveScreen?
    /// The terminal screen reports what it shows, nil once the user leaves it
    /// or disconnects that machine, so a relaunch returns only to a terminal
    /// the user was still using (plan 12.1: navigation is persisted).
    func showing(_ screen: LiveScreen?) {
        if let screen, let data = try? JSONEncoder().encode(screen) {
            UserDefaults.standard.set(data, forKey: Self.liveScreenKey)
        } else { UserDefaults.standard.removeObject(forKey: Self.liveScreenKey) }
    }
    private func restoreLiveScreen() {
        guard returning == nil, pendingImport == nil,
              let data = UserDefaults.standard.data(forKey: Self.liveScreenKey),
              let screen = try? JSONDecoder().decode(LiveScreen.self, from: data),
              workspaces.contains(where: { $0.id == screen.workspaceID }) else { return }
        returning = screen
    }
    private func startObservations(restore: OfflineRestore<AppleCryptography>) {
        let channel = AsyncStream<CameraObservation>.makeStream(bufferingPolicy: .bufferingNewest(8))
        observations = channel.continuation
        observationTask = Task { [weak self] in
            for await observation in channel.stream {
                guard !Task.isCancelled, let self else { break }
                guard self.active, self.showRestore, self.epoch == observation.epoch else { continue }
                let before = await restore.status()
                guard self.active, self.showRestore, self.epoch == observation.epoch else { continue }
                do {
                    let next = try await restore.observe(observation.text, generation: observation.generation)
                    guard self.active, self.epoch == observation.epoch else { continue }
                    await self.saveScanSnapshot(next, restore: restore, epoch: observation.epoch)
                    guard self.active, self.epoch == observation.epoch else { continue }
                    let latest = await restore.status()
                    guard self.active, self.epoch == observation.epoch else { continue }
                    if latest.generation != next.generation { self.lastObservation = nil }
                    self.state = latest
                    if !self.keepsCamera(latest) { self.cameraRequested = false }
                    self.advanceAutomatically()
                } catch FTError.stale { continue }
                catch is CancellationError { break }
                catch {
                    var latest = await restore.status()
                    guard self.active, self.epoch == observation.epoch else { continue }
                    // Only receiver resets need an immediate write. Malformed
                    // framing/checksums leave it unchanged and use the normal
                    // cadence. Join even a manual writer so a reset cannot be
                    // lost if later equations restore the same progress count.
                    let repaired = before.generation == latest.generation
                        && (latest.receivedParts < before.receivedParts || latest.verifiedPages < before.verifiedPages)
                    await self.saveScanSnapshot(latest, restore: restore, epoch: observation.epoch, force: repaired)
                    guard self.active, self.epoch == observation.epoch else { continue }
                    latest = await restore.status()
                    guard self.active, self.epoch == observation.epoch else { continue }
                    if self.state?.generation != latest.generation { self.lastObservation = nil }
                    self.state = latest
                    if ![.idle, .candidate, .collecting].contains(latest.phase) { self.cameraRequested = false }
                    self.report(error)
                }
            }
        }
    }
    /// A separately revealed code scanned deliberately from the Mac, bound to
    /// this transfer. Saved recordings never feed this camera-only entry point.
    @ObservationIgnored private var scannedCode: (digestPrefix: String, code: String)?
    /// Bounded witness for this receive generation.
    /// Resetting or resuming the same envelope must not suppress its selection.
    @ObservationIgnored private var automaticSelection: (generation: UUID, digest: Data)?
    /// Collect the transfer in view and accept a separately scanned unlock
    /// code. Installation always stops for the user's source/contents review.
    private func advanceAutomatically() {
        guard !busy, !canRetryScanSave, active, showRestore, let state, !state.isSuspended else { return }
        switch state.phase {
        case .candidate:
            guard let digest = state.identity?.envelopeDigest,
                  automaticSelection?.generation != state.generation || automaticSelection?.digest != digest else { return }
            automaticSelection = (state.generation, digest)
            run { restore, state in try await restore.selectCandidate(generation: state.generation, digest: digest) }
        case .locked where state.encryptionMode == .code:
            guard let scanned = scannedCode, let digest = state.identity?.envelopeDigest,
                  scanned.digestPrefix == TransferCodeFrame.digestPrefix(digest) else { return }
            scannedCode = nil
            let code = scanned.code
            run { restore, state in try await restore.unlock(enteredCode: code, generation: state.generation) }
        default: break
        }
    }

    /// Starts over from a finished restore, which gave up offline exclusion:
    /// resets and takes exclusion back before anything is scanned or read.
    private func restartFinished(_ restore: OfflineRestore<AppleCryptography>) async throws -> OfflineRestoreStatus {
        let ticket = epoch
        scannedCode = nil; stopSavingScanProgress()
        guard active, showRestore, epoch == ticket else { throw CancellationError() }
        _ = try await restore.reset()
        guard active, showRestore, epoch == ticket else { throw CancellationError() }
        do { try await restore.enterOfflineMode() }
        catch {
            // A completed import released exclusion, so starting the next one
            // may meet sessions opened since then. Offer the same explicit
            // disconnect choice as initial entry, rather than a stale receipt.
            let paused = await restore.suspend()
            if active, showRestore, epoch == ticket {
                cameraRequested = false; offlineModeBlocked = true
                state = paused; report(error)
            }
            throw error
        }
        return await restore.status()
    }
    /// Keep the camera while a code-protected transfer still waits for its code.
    private func keepsCamera(_ status: OfflineRestoreStatus) -> Bool {
        [.idle, .candidate, .collecting].contains(status.phase)
            || (status.phase == .locked && status.encryptionMode == .code && scannedCode == nil)
    }

    /// At most eight bounded strings wait on one consumer. No task-per-frame fanout.
    func observed(_ text: String) {
        if text.hasPrefix("FTCODE1:") {
            guard active, showRestore, cameraRequested, let frame = TransferCodeFrame.parse(text) else { return }
            if scannedCode?.digestPrefix != frame.digestPrefix || scannedCode?.code != frame.code {
                scannedCode = frame
                advanceAutomatically()
            }
            return
        }
        guard active, showRestore, cameraRequested, !busy, let state, !state.isSuspended,
              [.idle, .candidate, .collecting].contains(state.phase), text.hasPrefix("FTQR1/"),
              text.utf8.count <= FTLimit.qrText, text != lastObservation else { return }
        let now = ContinuousClock.now
        guard lastObservationTime.duration(to: now) >= .milliseconds(35) else { return }
        lastObservation = text; lastObservationTime = now
        observations?.yield(.init(text: text, generation: state.generation, epoch: epoch))
    }
    func run(_ action: @escaping @Sendable (OfflineRestore<AppleCryptography>, OfflineRestoreStatus) async throws -> OfflineRestoreStatus) {
        guard !busy, active, let restore else { return }
        guard !canRetryScanSave else {
            scanProgressMessage = "Retry Saving or choose Continue Without Saving before changing this scan."
            return
        }
        busy = true; message = nil; cancelScanSaveTimer()
        let ticket = epoch, pendingLifecycle = lifecycle
        Task { [weak self] in
            await pendingLifecycle?.value
            guard let self else { return }
            await self.scanWrite?.task.value
            defer {
                self.busy = false
                self.openPendingFileIfReady()
                self.advanceAutomatically()
            }
            guard self.active, self.epoch == ticket, !self.offlineModeBlocked else { return }
            guard !self.canRetryScanSave else {
                self.scanProgressMessage = "Retry Saving or choose Continue Without Saving before changing this scan."
                return
            }
            do {
                let current = await restore.status()
                let result = try await action(restore, current)
                guard self.active, self.epoch == ticket else { return }
                if current.generation != result.generation { self.lastObservation = nil }
                self.state = result
                await self.saveScanSnapshot(result, restore: restore, epoch: ticket, force: true)
                guard self.active, self.epoch == ticket else { return }
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
        scannedCode = nil
        if !OpticalRecordingLimits.accepts(url) { stopSavingScanProgress() }
        pendingImport = PendingFileImport(url: url)
        guard active, restore != nil else {
            if !bootstrapStarted { Task { await bootstrap() } }
            return
        }
        cameraRequested = false
        if OpticalRecordingLimits.accepts(url), showRestore, state?.isSuspended == false {
            // Keep the active scan generation and its opted-in checkpoint owner
            // when adding recording frames to an unfinished camera transfer.
            openPendingFileIfReady()
        } else { setRestoreVisible(true) }
        // The serialized lifecycle calls openPendingFileIfReady after obtaining
        // the actor's current status. A paused phone request keeps its selection.
    }
    private func openPendingFileIfReady() {
        guard active, showRestore, !busy, let restore, let selected = pendingImport,
              !selected.needsExplicitRetry, state?.isSuspended == false else { return }
        if OpticalRecordingLimits.accepts(selected.url) {
            startRecording(selected, restore: restore)
            return
        }
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
                let ready = current.phase == .complete ? try await self.restartFinished(restore) : current
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

    /// The selected reader has one owner until cancellation and native cleanup
    /// have joined. Each decoded frame is backpressured by the existing receiver
    /// and optional checkpoint writer; no per-frame tasks or image queue exist.
    private func startRecording(_ selected: PendingFileImport, restore: OfflineRestore<AppleCryptography>) {
        guard recordingRead == nil, !canRetryScanSave else {
            pendingImport?.needsExplicitRetry = true
            scanProgressMessage = "Retry Saving or Continue Without Saving before reading the selected recording."
            return
        }
        isReadingRecording = true; recordingProgress = nil; message = nil; cameraRequested = false
        observations?.finish(); observations = nil
        let priorObservations = observationTask
        priorObservations?.cancel(); observationTask = nil; lastObservation = nil
        let ticket = epoch, pendingLifecycle = lifecycle
        let reader = NativeOpticalRecording()
        let job = Task { [weak self] in
            await pendingLifecycle?.value
            await priorObservations?.value
            guard let self else { return }
            defer {
                if self.recordingRead?.id == selected.id {
                    self.recordingRead = nil; self.isReadingRecording = false
                    if self.active, self.showRestore, self.epoch == ticket {
                        self.advanceAutomatically()
                    }
                }
            }
            do {
                try self.checkRecordingOwner(selected.id, epoch: ticket)
                let current = await restore.status()
                let ready = current.phase == .complete ? try await self.restartFinished(restore) : current
                try self.checkRecordingOwner(selected.id, epoch: ticket)
                guard [.idle, .candidate, .collecting].contains(ready.phase), !ready.isSuspended else {
                    throw WorkspaceStoreError.busy
                }
                self.state = ready
                let result = try await reader.read(selected.url) { [weak self] texts, progress in
                    guard let self else { throw CancellationError() }
                    try await self.checkRecordingOwner(selected.id, epoch: ticket)
                    let next = try await restore.observeRecordingFrame(texts, generation: ready.generation)
                    return try await self.receivedRecordingFrame(next, progress: progress,
                        restore: restore, id: selected.id, epoch: ticket)
                }
                try self.checkRecordingOwner(selected.id, epoch: ticket)
                let latest = await restore.status()
                await self.saveScanSnapshot(latest, restore: restore, epoch: ticket, force: true)
                try self.checkRecordingOwner(selected.id, epoch: ticket)
                self.state = latest; self.pendingImport = nil
                if latest.phase == .locked {
                    self.message = "Encrypted transfer recovered. Unlock it using the separately saved code or this phone's active recipient request, then review what will be restored."
                } else if self.canRetryScanSave {
                    self.message = "Reading paused because saving progress needs attention. Your recovered pieces remain available."
                } else if latest.identity == nil {
                    self.message = "No usable transfer symbols were found in \(result.framesRead) frames. Choose a QR recording exported by FrankenTerm."
                } else {
                    self.message = "Read \(result.framesRead) frames. This recording did not complete the transfer. Recovered pieces are kept; read another recording of the same transfer or resume the camera and use Repair Page on your Mac."
                }
            } catch {
                let latest = await restore.status()
                let interrupted = Task.isCancelled || !self.active || self.epoch != ticket || latest.isSuspended
                if self.pendingImport?.id == selected.id {
                    if interrupted { self.markPendingImportInterrupted(selected.id) }
                    else { self.pendingImport = nil }
                }
                guard self.active, self.showRestore, self.epoch == ticket else { return }
                self.state = latest
                // A malformed page can reduce receiver progress, and explicit
                // Stop cancels this reader task. Join a separate settlement so
                // an opted-in cache records the exact remaining decoder, even
                // on cancellation. Background/dismissal revokes its authority
                // through the normal active/epoch/save-owner checks.
                let settlement = Task { [weak self] in
                    guard let self, self.recordingRead?.id == selected.id,
                          self.active, self.showRestore, self.epoch == ticket else { return }
                    await self.saveScanSnapshot(latest, restore: restore, epoch: ticket, force: true)
                }
                await settlement.value
                guard self.active, self.showRestore, self.epoch == ticket,
                      self.recordingRead?.id == selected.id else { return }
                if interrupted { self.message = "Reading stopped. Recovered pieces are kept; continue with another recording or the camera." }
                else { self.report(error) }
            }
        }
        recordingRead = (selected.id, job)
    }

    private func checkRecordingOwner(_ id: UUID, epoch ticket: UUID) throws {
        try Task.checkCancellation()
        guard active, showRestore, epoch == ticket, recordingRead?.id == id,
              pendingImport?.id == id else { throw CancellationError() }
    }

    private func receivedRecordingFrame(_ next: OfflineRestoreStatus, progress: OpticalRecordingProgress,
        restore: OfflineRestore<AppleCryptography>, id: UUID, epoch ticket: UUID) async throws -> Bool {
        try checkRecordingOwner(id, epoch: ticket)
        state = next; recordingProgress = progress
        await saveScanSnapshot(next, restore: restore, epoch: ticket)
        try checkRecordingOwner(id, epoch: ticket)
        return [.idle, .candidate, .collecting].contains(next.phase) && !canRetryScanSave
    }

    func stopReadingRecording() {
        guard let current = recordingRead else { return }
        // Retain and join the reader before allowing another input. Clearing
        // the pending choice also prevents an automatic replay after Stop.
        pendingImport = nil
        current.task.cancel()
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
            } else if error == .malformed("recording.ambiguous_sources") {
                message = "This recording shows multiple different transfers at once. No source was selected. Choose a recording with one transfer."
            } else if error == .unsupported("file.not_local") {
                message = "This file is not downloaded on this phone. Choose an already-local copy for offline restore."
            } else { message = error.localizedDescription }
        }
        else if let error = error as? WorkspaceStoreError { message = error.localizedDescription }
        else if let error = error as? WorkspaceNetworkError { message = error.localizedDescription }
        else if !(error is CancellationError) {
            // Say what failed: a generic message hid the Keychain status a
            // person (or a bug report) needs.
            let detail = (error as? LocalizedError)?.errorDescription ?? String(describing: error)
            message = "The restore did not finish. \(detail)"
        }
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
                Group {
                    if model.workspaces.isEmpty { firstRun } else { savedWorkspaces }
                }
                .navigationDestination(item: $model.returning) { screen in
                    if let saved = model.workspaces.first(where: { $0.id == screen.workspaceID }),
                       let document = try? saved.document() {
                        MachineWorkspaceView(initialDocument: document, workspaceID: saved.id, model: model,
                                             returningMachineID: screen.machineID)
                    }
                }
                .navigationTitle("FrankenTerm")
                .toolbar {
                    ToolbarItem(placement: .primaryAction) {
                        Menu {
                            Button { model.requestCamera() } label: { Label("Scan My Mac", systemImage: "qrcode.viewfinder") }
                            Button { chooseFile = true } label: { Label("Open a Workspace File", systemImage: "doc") }
                            Button { model.requestRecipient() } label: {
                                Label("Encrypt Only for This Phone", systemImage: "lock.iphone")
                            }.disabled(model.busy || ![.idle, .complete].contains(model.state?.phase ?? .idle))
                            if !model.savedScans.isEmpty {
                                Button { model.setRestoreVisible(true) } label: {
                                    Label("Saved Scan", systemImage: "qrcode")
                                }.disabled(model.busy)
                            }
                        } label: { Image(systemName: "plus") }
                            .disabled(model.state == nil)
                            .accessibilityLabel("Add machines")
                    }
                }
            }
            if !model.active { Color(.systemBackground).ignoresSafeArea().overlay { Label("FrankenTerm", systemImage: "lock.shield").font(.title2) } }
        }
        .task { model.setScenePhase(scenePhase); await model.bootstrap() }
        .onChange(of: scenePhase) { _, phase in model.setScenePhase(phase) }
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

    /// Nothing saved yet: one sentence and one button.
    private var firstRun: some View {
        VStack(spacing: 28) {
            Spacer()
            Image(systemName: "laptopcomputer.and.iphone")
                .font(.system(size: 72, weight: .light)).symbolRenderingMode(.hierarchical).foregroundStyle(.tint)
            VStack(spacing: 10) {
                Text("Bring your machines over").font(.title.bold())
                Text("On your Mac, open FrankenTerm Export and click Send to iPhone. Then scan it here.")
                    .font(.body).foregroundStyle(.secondary).multilineTextAlignment(.center)
            }
            Spacer()
            Button { model.requestCamera() } label: {
                Label("Scan My Mac", systemImage: "qrcode.viewfinder")
                    .font(.headline).frame(maxWidth: .infinity).padding(.vertical, 8)
            }.buttonStyle(.borderedProminent).controlSize(.large)
                .disabled(model.state == nil || model.requestingCameraAccess)
            if !model.savedScans.isEmpty {
                Button("Resume a Saved Scan") { model.setRestoreVisible(true) }
                    .disabled(model.state == nil || model.busy)
            }
        }.padding(24).padding(.bottom, 12)
    }

    private var savedWorkspaces: some View {
        List(model.workspaces) { saved in
            if let document = try? saved.document() {
                NavigationLink { MachineWorkspaceView(initialDocument: document, workspaceID: saved.id, model: model) } label: {
                    VStack(alignment: .leading, spacing: 8) {
                        Text(verbatim: document.name).font(.headline).lineLimit(1)
                        HStack(spacing: 5) {
                            ForEach(document.machines.prefix(12)) { machine in
                                let theme = model.machineThemes(saved.id, document: document)?.theme(for: machine)
                                    ?? document.themes.first { $0.id == machine.themeID }
                                Circle().fill(Color(ftHex: theme?.accent ?? "#888888")).frame(width: 10, height: 10)
                            }
                            Text("\(document.machines.count) machines").font(.caption).foregroundStyle(.secondary).padding(.leading, 4)
                        }
                    }.padding(.vertical, 6)
                }
            }
        }
    }
}

@MainActor
struct OfflineRestoreSheet: View {
    @Bindable var model: MobileWorkspaceModel
    @State private var code = ""
    @State private var repairPage = 0
    @State private var chooseFile = false
    private enum RestoreDecision {
        case save(generation: UUID, identity: OpticalFrame.Identity, replacing: OpticalDecoderRecoveryReview?)
        case discard(SavedOpticalScan)
        case disconnect(RestoreSessionDisconnectReview)
        var title: String {
            switch self {
            case .save: "Save Scan Progress?"
            case .discard: "Discard Saved Scan?"
            case .disconnect: "Disconnect Sessions and Restore?"
            }
        }
        var cancelTitle: String {
            switch self {
            case .disconnect: "Keep Sessions"
            case .save, .discard: "Cancel"
            }
        }
    }
    @State private var restoreDecision: RestoreDecision?
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
                        if model.busy { ProgressView("Preparing offline restore…") }
                        if model.canReviewSessionsForRestore {
                            Button("Disconnect Sessions and Restore", role: .destructive) {
                                if let reviewed = model.reviewSessionsForRestore() {
                                    restoreDecision = .disconnect(reviewed)
                                }
                            }
                        }
                        Button("Retry Offline Restore") { model.retryOfflineMode() }.buttonStyle(.borderedProminent)
                        Button("Cancel Restore") { model.setRestoreVisible(false) }
                    }.disabled(model.busy)
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
                            if let message = model.scanProgressMessage {
                                VStack(alignment: .leading, spacing: 8) {
                                    Label { Text(verbatim: message) } icon: { Image(systemName: "externaldrive.badge.exclamationmark") }
                                    Button("Dismiss Scan Message") { model.scanProgressMessage = nil }
                                }.font(.callout).padding().background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
                            }
                            if model.isReadingRecording {
                                recordingProgressContent
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
                            if [.idle, .candidate, .collecting, .locked].contains(state.phase), model.cameraRequested, !state.isSuspended {
                                if DataScannerViewController.isSupported && DataScannerViewController.isAvailable {
                                    OpticalCamera(onCode: model.observed, onError: model.report)
                                        .frame(maxWidth: .infinity).frame(height: 380)
                                        .clipShape(RoundedRectangle(cornerRadius: 24))
                                        .overlay(alignment: .bottom) {
                                            Label(state.phase == .locked ? "Scan the separately revealed transfer code" : "Point at the animation on your Mac", systemImage: "viewfinder")
                                                .font(.footnote.weight(.medium)).padding(.horizontal, 12).padding(.vertical, 7)
                                                .background(.ultraThinMaterial, in: Capsule()).padding(14)
                                        }
                                } else {
                                    Text("Camera scanning is unavailable. Allow Camera access in Settings or open an already-local .ftairgap file.")
                                }
                            }
                            switch state.phase {
                            case .idle:
                                idleContent(state)
                            case .candidate:
                                ProgressView("Found your Mac…")
                            case .collecting:
                                collectingContent(state)
                            case .locked:
                                lockedContent(state)
                            case .documentReady:
                                Text("The public workspace is ready for a fresh source and settings review. It contains no private keys and needs no transfer code.")
                                Button("Review Workspace") {
                                    model.run { restore, state in try await restore.reviewWorkspaceDocument(generation: state.generation) }
                                }.buttonStyle(.borderedProminent)
                            case .preparingReview, .committing:
                                ProgressView(state.phase == .committing ? "Saving your machines…" : "Unlocking…")
                                    .frame(maxWidth: .infinity).padding(.vertical, 24)
                            case .review:
                                if let review = state.review { reviewSummary(review) }
                            case .complete:
                                completeContent(state)
                            case .recoveryRequired:
                                Text("The save may have completed. Check the saved result before attempting another import.")
                                Button("Reconcile Saved Workspace") {
                                    model.run { restore, state in try await restore.reconcile(generation: state.generation) }
                                }.buttonStyle(.borderedProminent)
                            }
                            }
                            scanProgressContent(state)
                            ForEach(model.savedScans) { saved in savedScanContent(saved, state: state) }
                            if [.candidate, .collecting, .locked, .documentReady, .review].contains(state.phase)
                                || (state.phase == .idle && state.recipientRequest != nil) {
                                Button("Start Over", role: .destructive) {
                                    code = ""; model.cameraRequested = false
                                    model.resetRestore()
                                }
                            }
                        }.padding(24).disabled(model.busy)
                            .animation(.smooth, value: state.phase)
                            .sensoryFeedback(.success, trigger: state.phase == .complete)
                    }
                } else { ProgressView("Opening protected workspace…") }
            }.navigationTitle("Restore").navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        if model.isReadingRecording {
                            Button("Stop Reading") { model.stopReadingRecording() }
                        } else {
                            Button("Close") { model.setRestoreVisible(false) }.disabled(model.busy)
                        }
                    }
                }
        }
        .interactiveDismissDisabled(model.busy)
        .confirmationDialog(restoreDecision?.title ?? "Restore",
                            isPresented: Binding(get: { restoreDecision != nil }, set: { if !$0 { restoreDecision = nil } }),
                            titleVisibility: .visible, presenting: restoreDecision) { decision in
            switch decision {
            case .save(let generation, let identity, let reviewed):
                Button("Save Encrypted Progress") { model.saveScanProgress(generation: generation, identity: identity, replacing: reviewed) }
                    .disabled(model.busy || !model.active)
            case .discard(let saved):
                Button("Discard Saved Scan", role: .destructive) { model.discardSavedScan(saved) }
                    .disabled(model.busy || !model.active)
            case .disconnect(let reviewed):
                Button("Disconnect Sessions and Restore", role: .destructive) { model.disconnectSessionsAndRestore(reviewed) }
                    .disabled(model.busy || !model.active)
            }
            Button(decision.cancelTitle, role: .cancel) { restoreDecision = nil }
        } message: { decision in
            switch decision {
            case .save(_, _, let reviewed):
                Text("Keep this transfer's encrypted pages and unfinished reconstruction progress on this phone. Progress is saved about every two seconds while scanning, and when you choose Save and Pause Scan. Transfer codes and phone request keys are never saved. The cache is device-protected and excluded from backups."
                     + (reviewed == nil ? "" : " This updates the reviewed saved scan to your current progress, including any page repairs.")
                     + " Saving stops when you leave restore or the app becomes inactive; a sudden interruption can lose progress since the last save.")
            case .discard(let saved):
                Text("Discard the saved pages and unfinished progress for transfer \(TransferCodeFrame.digestPrefix(saved.identity.envelopeDigest))? Your current scan and installed workspaces stay available. Saving further progress will stop.")
            case .disconnect(let reviewed):
                Text(verbatim: "Disconnect sessions in these workspaces:\n\n"
                    + reviewed.workspaceNames.joined(separator: "\n")
                    + "\n\nDirect SSH disconnection can terminate remote shells and programs. Input already sent may have executed. Other network activity may still prevent offline restore.")
            }
        }
        .fileImporter(isPresented: $chooseFile, allowedContentTypes: [.json, .data, .movie, .png], allowsMultipleSelection: false) { result in
            switch result {
            case .success(let urls): if let url = urls.first { model.importFile(url) }
            case .failure(let error): model.report(error)
            }
        }
        .onChange(of: model.active) { _, active in
            if !active {
                code = ""
                restoreDecision = nil
            }
        }
        .onDisappear {
            code = ""; model.cameraRequested = false
            restoreDecision = nil
        }
    }

    @ViewBuilder private var recordingProgressContent: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let progress = model.recordingProgress {
                if let total = progress.totalFrames {
                    ProgressView(value: Double(progress.framesRead), total: Double(total)) {
                        Text("Reading QR recording")
                    }
                    Text("\(progress.framesRead) of \(total) frames examined").monospacedDigit()
                } else if let elapsed = progress.elapsedSeconds, let duration = progress.durationSeconds {
                    ProgressView(value: elapsed, total: duration) { Text("Reading QR recording") }
                    Text("\(progress.framesRead) frames examined").monospacedDigit()
                }
            } else { ProgressView("Opening local QR recording…") }
            Text("Recording progress measures frames read. Transfer recovery below measures encrypted pieces recovered. Stop Reading keeps your recovered pieces.")
                .font(.footnote).foregroundStyle(.secondary)
        }
    }

    @ViewBuilder private func scanProgressContent(_ state: OfflineRestoreStatus) -> some View {
        if state.source == .optical, [.collecting, .locked, .review].contains(state.phase),
           !state.isSuspended, !state.requiresRecipientResume, let identity = state.identity {
            VStack(alignment: .leading, spacing: 10) {
                if model.canRetryScanSave {
                    Label("Saving needs your attention", systemImage: "externaldrive.badge.exclamationmark").font(.headline)
                    Button("Retry Saving") { model.retryScanSave() }
                    Button("Continue Without Saving") { model.continueWithoutSaving() }
                } else if model.isSavingScanProgress {
                    Label("Keeping scan progress", systemImage: "externaldrive.badge.checkmark").font(.headline)
                    Text("Encrypted progress is saved about every two seconds, even before a page finishes. Save and pause before leaving to keep the latest progress.")
                        .font(.footnote).foregroundStyle(.secondary)
                    Button("Save and Pause Scan") { model.saveAndPauseScan() }
                    Button("Continue Without Saving") { model.continueWithoutSaving() }
                } else {
                    let existing = model.savedScans.compactMap { saved -> OpticalDecoderRecoveryReview? in
                        if case .partial(let value) = saved { return value }
                        return nil
                    }.first
                    if existing == nil || existing?.identity == identity {
                        Button {
                            restoreDecision = .save(generation: state.generation, identity: identity, replacing: existing)
                        } label: { Label(existing == nil ? "Save Scan Progress" : "Continue Saving This Scan", systemImage: "square.and.arrow.down") }
                        Text("Keep encrypted progress so an interrupted scan can continue later, even if no page has finished.")
                            .font(.footnote).foregroundStyle(.secondary)
                    } else {
                        Text("Another transfer has saved progress. Review or discard that saved scan before saving this one.")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                }
            }
        }
    }

    private func savedScanContent(_ saved: SavedOpticalScan, state: OfflineRestoreStatus) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            if case .verified = saved { Label("Saved Pages", systemImage: "qrcode").font(.headline) }
            else { Label("Saved Scan", systemImage: "qrcode").font(.headline) }
            Text("\(saved.verifiedPages) of \(saved.identity.pageCount) pages verified · \(ByteCountFormatter.string(fromByteCount: Int64(saved.verifiedBytes), countStyle: .file))")
                .monospacedDigit()
            if saved.independentParts > 0 {
                Text("\(saved.independentParts) recovered pieces of unfinished pages also saved")
                    .font(.callout).monospacedDigit()
            }
            Text(verbatim: "Transfer \(TransferCodeFrame.digestPrefix(saved.identity.envelopeDigest))")
                .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
            if [.idle, .complete].contains(state.phase), !state.requiresRecipientResume {
                Text("Resume this encrypted progress and keep saving as you scan. The camera opens only when you tap Scan or Resume Camera. A complete saved scan still needs to be unlocked.")
                    .font(.footnote).foregroundStyle(.secondary)
                Button("Resume Saved Scan") {
                    code = ""
                    model.resumeSavedScan(saved)
                }.buttonStyle(.borderedProminent)
            } else if state.identity != saved.identity {
                Text("Start over with the current restore before resuming this saved scan.")
                    .font(.footnote).foregroundStyle(.secondary)
            }
            Text("Codes and phone request keys are not saved. If an Only This Phone request key was lost when the app closed, create a new request and export again from your Mac.")
                .font(.footnote).foregroundStyle(.secondary)
            Button("Discard Saved Scan", role: .destructive) { restoreDecision = .discard(saved) }
        }.padding().frame(maxWidth: .infinity, alignment: .leading)
            .background(.quaternary, in: RoundedRectangle(cornerRadius: 12))
    }

    /// The payoff: what arrived, in its colors, and one way out.
    @ViewBuilder private func completeContent(_ state: OfflineRestoreStatus) -> some View {
        let document = try? state.receipt?.workspace.document()
        VStack(spacing: 18) {
            Image(systemName: "checkmark.circle.fill").font(.system(size: 64)).foregroundStyle(.green)
                .symbolEffect(.bounce, value: state.phase)
            Text(document.map { "\($0.machines.count) machines added" } ?? "Machines added").font(.title2.bold())
            if let document {
                VStack(spacing: 8) {
                    ForEach(document.machines) { machine in
                        let theme = document.themes.first { $0.id == machine.themeID }
                        HStack(spacing: 10) {
                            Circle().fill(Color(ftHex: theme?.accent ?? "#888888")).frame(width: 10, height: 10)
                            Text(verbatim: machine.name).foregroundStyle(Color(ftHex: theme?.foreground ?? "#FFFFFF")).lineLimit(1)
                            Spacer(minLength: 0)
                        }
                        .padding(.horizontal, 14).padding(.vertical, 10)
                        .background(Color(ftHex: theme?.background ?? "#1C1C1E"), in: RoundedRectangle(cornerRadius: 12))
                    }
                }
            }
            if let receipt = state.receipt, !receipt.missingCredentialIDs.isEmpty {
                Text("\(receipt.missingCredentialIDs.count) keys were not included, so their machines cannot connect yet.")
                    .font(.footnote).foregroundStyle(.secondary).multilineTextAlignment(.center)
            }
            Button { model.setRestoreVisible(false) } label: {
                Text("Done").font(.headline).frame(maxWidth: .infinity).padding(.vertical, 6)
            }.buttonStyle(.borderedProminent).controlSize(.large)
        }.frame(maxWidth: .infinity)
    }

    @ViewBuilder private func idleContent(_ state: OfflineRestoreStatus) -> some View {
        if let request = state.recipientRequest {
            RecipientRequestCard(request: request)
            Text("Hold this screen up to your Mac's camera. Then point this phone at the animation on your Mac.")
            Button("Scan My Mac") { model.requestCamera() }
                .buttonStyle(.borderedProminent).disabled(model.requestingCameraAccess)
        } else {
            Text("On your Mac, click Send to iPhone in FrankenTerm Export. Point this phone at the animation, unlock with the separately revealed code, then review your workspace.")
            if !model.cameraRequested {
                Button("Scan My Mac") { model.requestCamera() }
                    .buttonStyle(.borderedProminent).controlSize(.large).disabled(model.requestingCameraAccess)
            }
        }
        DisclosureGroup("Other ways to restore") {
            VStack(alignment: .leading, spacing: 12) {
                if state.recipientRequest == nil {
                    Button("Encrypt Only for This Phone (uses your Mac's camera)") { model.requestRecipient() }
                }
                Button(state.recipientRequest == nil ? "Open a Workspace File" : "Open an Encrypted File") {
                    chooseFile = true
                }
                Button("Restore from QR Recording") { chooseFile = true }
                Text("Read an already-local MP4 or animated PNG. Keep its transfer code separately; a recording alone cannot unlock the workspace.")
                    .font(.footnote).foregroundStyle(.secondary)
            }.padding(.top, 8)
        }
    }

    @ViewBuilder private func collectingContent(_ state: OfflineRestoreStatus) -> some View {
        let total = max(state.requiredParts, 1), received = min(state.receivedParts, total)
        ProgressView(value: Double(received), total: Double(total)) {
            Text("Recovering encrypted transfer")
        } currentValueLabel: {
            Text("\(Int((Double(received) / Double(total) * 100).rounded(.down)))%").monospacedDigit()
                .contentTransition(.numericText())
        }.animation(.smooth, value: received)
        if !model.cameraRequested {
            Button("Resume Camera") { model.requestCamera() }
                .buttonStyle(.borderedProminent).disabled(model.requestingCameraAccess)
        }
        Button("Read Another QR Recording") { chooseFile = true }
        if !state.incompletePageIndices.isEmpty {
            DisclosureGroup("Stuck? Repair a page") {
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
    }

    @ViewBuilder private func lockedContent(_ state: OfflineRestoreStatus) -> some View {
        if state.encryptionMode == .recipient {
            if state.canUnlockRecipient {
                Button("Unlock") {
                    model.run { restore, state in try await restore.unlockRecipient(generation: state.generation) }
                }.buttonStyle(.borderedProminent).controlSize(.large)
            } else {
                Text("This transfer was encrypted for a phone request that this session no longer holds. Start a different transfer and send it again from your Mac.")
            }
        } else if state.recipientRequest != nil {
            Text("This transfer was not encrypted for this phone. On your Mac, send it again after scanning this phone's request.")
        } else {
            Text("On your Mac, choose Show Transfer Code; the animation pauses while the code is visible. Enter that code here or scan its separately revealed QR. For a saved recording, use the code you kept separately.")
            SecureField("Transfer code", text: $code).textInputAutocapitalization(.characters).autocorrectionDisabled()
                .textFieldStyle(.roundedBorder).onChange(of: code) { _, value in if value.utf8.count > 128 { code = String(value.prefix(128)) } }
            Button("Unlock") {
                let entered = code; code = ""
                model.run { restore, state in try await restore.unlock(enteredCode: entered, generation: state.generation) }
            }.buttonStyle(.borderedProminent).disabled(code.isEmpty)
            if !model.cameraRequested {
                Button("Scan the Revealed Transfer Code") { model.requestCamera() }
                    .disabled(model.requestingCameraAccess)
            }
        }
    }

    /// What arrives, in its colors, and one Restore button; everything a
    /// person might audit is one tap away under Details.
    @ViewBuilder private func reviewSummary(_ review: OfflineRestoreReview) -> some View {
        Text(verbatim: review.document.name).font(.headline)
        let machines = review.document.machines.count, keys = review.includedFingerprints.count
        Text("\(machines) \(machines == 1 ? "machine" : "machines") · \(keys) \(keys == 1 ? "key" : "keys")")
            .foregroundStyle(.secondary)
        if let comparison = model.state?.comparison {
            Text(verbatim: "Transfer \(comparison)").font(.system(.body, design: .monospaced)).textSelection(.enabled)
            // Decrypting proves nothing about the sender; this code does.
            Text("This should match the code your Mac showed for this transfer.").font(.callout)
        }
        ForEach(review.document.machines) { machine in
            let theme = review.document.themes.first { $0.id == machine.themeID }
            HStack(spacing: 12) {
                RoundedRectangle(cornerRadius: 6).fill(Color(ftHex: theme?.background ?? "#000000"))
                    .overlay(Circle().fill(Color(ftHex: theme?.accent ?? "#777777")).padding(7))
                    .frame(width: 34, height: 26)
                Text(verbatim: machine.name).lineLimit(1)
            }
        }
        if let changes = review.changeReview { Text(verbatim: replacementSummary(review, changes)).font(.callout) }
        // Replace approves exactly what the line above says: this source
        // takes over the saved workspace with its changes, removals or
        // older version. Every changed value is under Details.
        Button(review.changeReview == nil ? "Restore" : "Replace") {
            let token = review.token, removal = review.removesMachines, rollback = review.isRollback
            let association = review.requiresSourceAssociation, security = review.requiresSecurityApproval
            model.run { restore, state in
                try await restore.commit(reviewToken: token, generation: state.generation,
                                         sourceAndContentsConfirmed: true, allowMachineRemoval: removal, allowRollback: rollback,
                                         associateWithExistingWorkspace: association, securityChangesConfirmed: security)
            }
        }.buttonStyle(.borderedProminent).controlSize(.large).frame(maxWidth: .infinity)
        DisclosureGroup("Details") { reviewDetails(review).padding(.top, 8) }
    }

    @ViewBuilder private func reviewDetails(_ review: OfflineRestoreReview) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            if model.state?.source == .workspaceDocument {
                Text("This file is not encrypted or signed. Matching keys already on this phone can be reused.")
                    .font(.caption)
            }
            ForEach(review.document.machines) { machine in
                Text(verbatim: machine.name).font(.caption.bold())
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
                WorkspaceReplacementReview(changes: changes, existingName: review.existingWorkspaceName ?? review.document.name)
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
        }
    }

    /// A replacement in one line: the saved workspace it takes over, how many
    /// settings change, and any removed machines or older version.
    private func replacementSummary(_ review: OfflineRestoreReview, _ changes: WorkspaceChangeReview) -> String {
        var effects = ["Replaces your saved “\(review.existingWorkspaceName ?? review.document.name)”"]
        if !changes.changes.isEmpty {
            let count = changes.changes.count, security = changes.securityChanges.count
            effects.append("changes \(count) \(count == 1 ? "setting" : "settings")"
                + (security == 0 ? "" : " (\(security) of them connections or host keys)"))
        }
        if review.removesMachines { effects.append("removes the machines it no longer has") }
        if review.isRollback { effects.append("goes back to an older version") }
        return ListFormatter.localizedString(byJoining: effects) + "."
    }

    private func title(_ phase: OfflineRestorePhase) -> String {
        switch phase {
        case .idle: "Restore from your Mac"
        case .candidate: "Found your Mac"
        case .collecting: "Receiving"
        case .locked: "Unlocking"
        case .documentReady, .preparingReview, .review: "Ready to restore"
        case .committing: "Saving"
        case .complete: "All set"
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
                        if route.mode == "frankenterm_mux" {
                            // What this route runs on the machine each time it connects.
                            Text(verbatim: "Runs on connect: \(route.remoteCommand ?? WorkspaceNativeSSHConnection.defaultMuxCommand)")
                                .font(.system(.caption2, design: .monospaced)).textSelection(.enabled)
                        }
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
                   text.hasPrefix("FTQR1/") || text.hasPrefix("FTCODE1:"), text.utf8.count <= FTLimit.qrText { onCode(text) }
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

typealias WorkspaceKeyPassphraseRequest = OpenSSHSourceKeyUnlocker<AppleCryptography>.Request

/// One bounded file/unlock owner. Private material never enters the observable
/// app model; file I/O and the synchronous bounded KDF stay off MainActor.
private actor WorkspaceKeyImportWorker {
    enum Selection: Sendable { case ready, passphrase(WorkspaceKeyPassphraseRequest) }
    private let store: WorkspaceStore
    private let review: WorkspaceCredentialImportReview
    private var unlocker = OpenSSHSourceKeyUnlocker(using: AppleCryptography())
    private var selected: UUID?
    private var key: PrivateKeyRecord?
    private var retired = false

    init(store: WorkspaceStore, review: WorkspaceCredentialImportReview) {
        self.store = store; self.review = review
    }

    func resetSelection(_ id: UUID) throws {
        try Task.checkCancellation()
        guard !retired else { throw FTError.stale }
        try unlocker.cancel(); key = nil; selected = id
    }

    func selectFile(_ url: URL, selection: UUID) throws -> Selection {
        try requireSelection(selection)
        var bytes = try SelectedLocalFile.read(url, limit: FTLimit.key)
        defer { bytes.resetBytes(in: 0..<bytes.count) }
        try requireSelection(selection)
        do {
            let result = try unlocker.select(pem: bytes, reference: review.credential)
            try requireSelection(selection)
            switch result {
            case .ready(let key): self.key = key; return .ready
            case .passphraseRequired(let request): return .passphrase(request)
            }
        } catch {
            try? unlocker.cancel(); key = nil
            throw error
        }
    }

    func unlock(selection: UUID, requestID: UUID, passphrase: Data) throws {
        try requireSelection(selection)
        var secret = passphrase
        defer { secret.resetBytes(in: 0..<secret.count) }
        let unlocked = try unlocker.unlock(requestID: requestID, passphrase: secret)
        try requireSelection(selection)
        key = unlocked
    }

    func pendingRequest(selection: UUID) -> WorkspaceKeyPassphraseRequest? {
        guard !retired, selected == selection else { return nil }
        return unlocker.request
    }

    func commit(selection: UUID) async throws -> WorkspaceCredentialImportReceipt {
        try requireSelection(selection)
        guard let key else { throw FTError.stale }
        // Final confirmation consumes this selected private value once. A
        // failed or cancelled write is reconciled by the model, never replayed.
        self.key = nil; selected = nil; retired = true
        try unlocker.cancel()
        return try await store.importCredential(key, review: review, confirmedByUser: true)
    }

    func retire() {
        retired = true; key = nil; selected = nil
        try? unlocker.cancel()
    }

    private func requireSelection(_ id: UUID) throws {
        try Task.checkCancellation()
        guard !retired, selected == id else { throw FTError.stale }
    }
}

@MainActor
@Observable
final class WorkspaceKeyImportController: Identifiable {
    let id = UUID()
    let workspaceID: UUID, revision: UUID, epoch: UUID
    let credential: WorkspaceDocument.Credential
    private(set) var review: WorkspaceCredentialImportReview?
    private(set) var passphraseRequest: WorkspaceKeyPassphraseRequest?
    private(set) var fileRequest: UUID?
    private(set) var isReady = false
    private(set) var isWorking = false
    private(set) var isCommitting = false
    private(set) var isFinished = false
    private(set) var isRetired = false
    private(set) var message: String?
    @ObservationIgnored private weak var model: MobileWorkspaceModel?
    @ObservationIgnored private let store: WorkspaceStore
    @ObservationIgnored private var worker: WorkspaceKeyImportWorker?
    @ObservationIgnored private var selection: UUID?
    @ObservationIgnored private var work: Task<Void, Never>?
    @ObservationIgnored private var retirement: Task<Void, Never>?

    init(model: MobileWorkspaceModel, store: WorkspaceStore, workspaceID: UUID,
         revision: UUID, credential: WorkspaceDocument.Credential, epoch: UUID) {
        self.model = model; self.store = store; self.workspaceID = workspaceID
        self.revision = revision; self.credential = credential; self.epoch = epoch
    }

    var canChooseFile: Bool {
        review != nil && !isWorking && !isFinished && !isRetired && fileRequest == nil
    }
    var canImport: Bool { isReady && !isWorking && !isFinished && !isRetired }

    func prepare() {
        guard review == nil, work == nil, !isRetired else { return }
        start { [self] in
            do {
                let reviewed = try await store.reviewCredentialImport(workspaceID: workspaceID,
                    expectedRevision: revision, credentialID: credential.id)
                try requireCurrent()
                review = reviewed; worker = WorkspaceKeyImportWorker(store: store, review: reviewed)
            } catch { report(error) }
        }
    }

    func chooseFile() {
        guard canChooseFile, let worker else { return }
        let chosen = UUID()
        selection = chosen; isReady = false; passphraseRequest = nil; message = nil
        start { [self] in
            do {
                try requireCurrent()
                try await worker.resetSelection(chosen)
                try requireCurrent()
                guard selection == chosen else { throw FTError.stale }
                fileRequest = chosen
            } catch { report(error) }
        }
    }

    func selectedFile(requestID: UUID, result: Result<[URL], Error>) {
        guard fileRequest == requestID, selection == requestID, !isRetired, let worker else { return }
        fileRequest = nil
        do {
            try requireCurrent()
            let urls = try result.get()
            guard urls.count == 1, let url = urls.first, url.isFileURL else {
                throw FTError.malformed("file.local_url_required")
            }
            start { [self] in
                do {
                    try requireCurrent()
                    let value = try await worker.selectFile(url, selection: requestID)
                    try requireCurrent()
                    guard selection == requestID else { throw FTError.stale }
                    switch value {
                    case .ready: isReady = true
                    case .passphrase(let request): passphraseRequest = request
                    }
                } catch { report(error) }
            }
        } catch {
            let failure = error as NSError
            if failure.domain != NSCocoaErrorDomain || failure.code != NSUserCancelledError { report(error) }
        }
    }

    func cancelledFile(requestID: UUID) {
        guard fileRequest == requestID, selection == requestID, !isRetired else { return }
        // resetSelection already discarded the previous key before this picker
        // appeared; cancelling it needs no file read or unlock attempt.
        fileRequest = nil; selection = nil
    }

    func unlock(_ passphrase: Data) {
        guard !isWorking, !isRetired, let request = passphraseRequest,
              let selection, let worker, (1...OpenSSHSourceKey.maximumPassphraseBytes).contains(passphrase.count) else { return }
        message = nil
        start { [self] in
            do {
                try requireCurrent()
                try await worker.unlock(selection: selection, requestID: request.id, passphrase: passphrase)
                try requireCurrent()
                guard self.selection == selection, passphraseRequest?.id == request.id else { throw FTError.stale }
                passphraseRequest = nil; isReady = true
            } catch {
                let remaining = await worker.pendingRequest(selection: selection)
                guard !isRetired else { return }
                if model?.credentialImportIsCurrent(self) == true, self.selection == selection,
                   passphraseRequest?.id == request.id {
                    passphraseRequest = remaining
                    report(error)
                } else { cancel() }
            }
        }
    }

    func importMatchingKey() {
        guard canImport, let worker, let selection, let model else { return }
        isReady = false; isCommitting = true; message = nil
        start { [self] in
            let result: Result<WorkspaceCredentialImportReceipt, Error>
            do {
                try requireCurrent()
                try await model.prepareCredentialImport(self)
                try requireCurrent()
                result = .success(try await worker.commit(selection: selection))
            } catch { result = .failure(error) }
            // An unstructured task does not inherit caller cancellation. Keep
            // it retained and joined so a published key always gets read back.
            let settlement = Task { await model.finishCredentialImport(self, result: result) }
            let outcome = await settlement.value
            await worker.retire()
            isCommitting = false; isFinished = true
            if !isRetired, model.credentialImportIsCurrent(self, allowChangedRevision: true) {
                message = outcome
            } else { cancel() }
        }
    }

    /// Retain the owner until its current work and secret retirement settle.
    /// A newer sheet cannot start another KDF while this one is still draining.
    func cancel(message: String? = nil) {
        guard !isRetired else { return }
        isRetired = true; isReady = false; passphraseRequest = nil; fileRequest = nil
        if let message { model?.credentialMessage = message }
        let pending = work, worker = worker
        pending?.cancel()
        retirement = Task { [self] in
            await pending?.value
            await worker?.retire()
            model?.releaseCredentialImport(self)
            retirement = nil
        }
    }

    private func start(_ operation: @escaping @MainActor () async -> Void) {
        guard work == nil, !isRetired else { return }
        isWorking = true
        work = Task { [self] in
            defer { work = nil; isWorking = false }
            await operation()
        }
    }

    private func requireCurrent() throws {
        try Task.checkCancellation()
        guard !isRetired, model?.credentialImportIsCurrent(self) == true else { throw FTError.stale }
    }

    private func report(_ error: Error) {
        guard !isRetired else { return }
        guard model?.credentialImportIsCurrent(self) == true, !(error is CancellationError) else {
            cancel(); return
        }
        message = Self.describe(error)
    }

    static func describe(_ error: Error) -> String {
        if error is FTError || error is OpenSSHSourceKeyError || error is WorkspaceStoreError
            || error is SSHAuthenticationError { return error.localizedDescription }
        return "The selected key file or protected storage could not be read. Choose a local OpenSSH private-key file and try again."
    }
}

/// A distinct view identity captures the picker ticket. A late result from a
/// dismissed Files controller cannot borrow a subsequent selection's ticket.
@MainActor
private struct WorkspaceKeyFilePicker: View {
    let requestID: UUID
    let selected: @MainActor (UUID, Result<[URL], Error>) -> Void
    let cancelled: @MainActor (UUID) -> Void
    @State private var presented = false
    var body: some View {
        Color.clear.frame(width: 0, height: 0)
            .fileImporter(isPresented: $presented, allowedContentTypes: [.data], allowsMultipleSelection: false) { [requestID, selected] result in
                Task { @MainActor in selected(requestID, result) }
            } onCancellation: { [requestID, cancelled] in
                Task { @MainActor in cancelled(requestID) }
            }
            .task { presented = true }
    }
}

@MainActor
private struct WorkspaceKeyImportSheet: View {
    @Bindable var controller: WorkspaceKeyImportController
    @State private var passphrase = ""
    @State private var passphraseLimit = false

    var body: some View {
        NavigationStack {
            Group {
                if controller.isRetired {
                    ContentUnavailableView("Import Closed", systemImage: "lock.shield")
                } else {
                    Form {
                        Section("Saved Identity") {
                            if let name = controller.review?.workspaceName { Text(verbatim: name).font(.headline) }
                            Text(verbatim: controller.credential.id)
                            Text(verbatim: controller.credential.algorithm).font(.caption).foregroundStyle(.secondary)
                            Text(verbatim: controller.credential.fingerprint)
                                .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                            if let aliases = controller.review?.credentialIDs, aliases.count > 1 {
                                Text("This identity also supplies \(aliases.count - 1) matching key references in this workspace.")
                                    .font(.caption)
                            }
                        }
                        if let message = controller.message {
                            Section { Text(verbatim: message) }
                        }
                        if controller.isWorking {
                            Section {
                                ProgressView(controller.isCommitting ? "Saving key and refreshing device status…" : "Reading the selected identity…")
                            }
                        }
                        if let request = controller.passphraseRequest, !controller.isFinished {
                            Section {
                                SecureField("Key File Passphrase", text: $passphrase)
                                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                                    .textContentType(.password).submitLabel(.done)
                                    .onSubmit(unlock)
                                if passphraseLimit {
                                    Text("The passphrase is limited to \(OpenSSHSourceKey.maximumPassphraseBytes) UTF-8 bytes.")
                                        .font(.caption).foregroundStyle(.secondary)
                                }
                                Button("Unlock Selected Key", action: unlock)
                                    .disabled(passphrase.isEmpty || controller.isWorking)
                                Text("\(request.attemptsRemaining) attempts remaining. The passphrase is used only to unlock this selected file on the phone.")
                                    .font(.caption)
                            } header: { Text("Encrypted OpenSSH Key") }
                        }
                        if controller.isReady {
                            Section {
                                Label("Private key matches the saved fingerprint", systemImage: "checkmark.shield")
                                Text("Import this key into protected storage on this phone. Existing sessions in this workspace will disconnect so its connection settings can refresh; their remote shells or programs may end. You choose when to connect again.")
                                    .font(.callout)
                                Button("Import Matching Key") { passphrase = ""; controller.importMatchingKey() }
                                    .disabled(!controller.canImport)
                                    .accessibilityIdentifier("keys.import.confirm")
                            }
                        }
                        if !controller.isFinished {
                            Section {
                                Button(controller.isReady || controller.passphraseRequest != nil ? "Choose Another Key File" : "Choose Key File") {
                                    passphrase = ""; passphraseLimit = false; controller.chooseFile()
                                }.disabled(!controller.canChooseFile)
                                    .accessibilityIdentifier("keys.import.choose")
                            } footer: {
                                Text("Choose a local OpenSSH private-key file for this exact fingerprint. Ed25519 and ECDSA P-256, P-384 and P-521 keys are supported, including files protected with bcrypt and AES-CTR.")
                            }
                        }
                    }
                }
            }
            .navigationTitle("Import SSH Key").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button(controller.isFinished ? "Done" : "Cancel") { passphrase = ""; controller.cancel() }
                }
            }
        }
        .privacySensitive()
        .interactiveDismissDisabled(controller.isCommitting)
        .background {
            if let request = controller.fileRequest {
                WorkspaceKeyFilePicker(requestID: request, selected: controller.selectedFile,
                                       cancelled: controller.cancelledFile).id(request)
            }
        }
        .task { controller.prepare() }
        .onChange(of: controller.passphraseRequest?.id) { _, _ in passphrase = ""; passphraseLimit = false }
        .onChange(of: passphrase) { old, value in
            if value.utf8.count > OpenSSHSourceKey.maximumPassphraseBytes {
                passphrase = old; passphraseLimit = true
            }
        }
        .onDisappear { passphrase = ""; controller.cancel() }
    }

    private func unlock() {
        guard !passphrase.isEmpty, passphrase.utf8.count <= OpenSSHSourceKey.maximumPassphraseBytes else { return }
        let bytes = Data(passphrase.utf8)
        passphrase = ""; passphraseLimit = false
        controller.unlock(bytes)
    }
}

@MainActor
private struct WorkspaceKeysSheet: View {
    let workspaceID: UUID
    @Bindable var model: MobileWorkspaceModel
    @Environment(\.dismiss) private var dismiss
    @State private var reviewed: LocalCredentialRevocationReview?

    private var document: WorkspaceDocument? {
        try? model.workspaces.first(where: { $0.id == workspaceID })?.document()
    }

    var body: some View {
        let importing = model.credentialImport.flatMap { $0.isRetired ? nil : $0 }
        NavigationStack {
            Group {
                if !model.active {
                    ContentUnavailableView("Locked", systemImage: "lock.shield",
                        description: Text("Return to FrankenTerm to manage keys."))
                } else if let document {
                    List {
                        if let message = model.credentialMessage {
                            Section {
                                Text(verbatim: message)
                                Button("Dismiss Message") { model.credentialMessage = nil }
                            }
                        }
                        Section {
                            ForEach(document.credentials) { key in
                                VStack(alignment: .leading, spacing: 10) {
                                    Text(verbatim: key.id).font(.headline)
                                    Text(verbatim: key.algorithm).font(.caption).foregroundStyle(.secondary)
                                    Text(verbatim: key.fingerprint).font(.system(.caption, design: .monospaced))
                                        .textSelection(.enabled)
                                    let pending = model.unconfirmedCredentialRevocations.contains(key.fingerprint)
                                    let revoked = model.locallyRevokedFingerprints.contains(key.fingerprint)
                                    if pending {
                                        Label("Blocked in this app; saving needs retry", systemImage: "exclamationmark.shield")
                                            .font(.callout)
                                    } else if revoked {
                                        Label("Revoked on this phone", systemImage: "key.slash").font(.callout)
                                    } else if model.availableCredentials[workspaceID]?.contains(key.id) == true {
                                        Label("Key present on this phone", systemImage: "key.fill").font(.callout)
                                    } else {
                                        Label("Key unavailable on this phone", systemImage: "key").font(.callout)
                                    }
                                    let routeCount = document.routes.filter { $0.credentialIDs.contains(key.id) }.count
                                    Text("Used by \(routeCount) routes in this workspace").font(.caption).foregroundStyle(.secondary)
                                    Button("Import Matching Key…", systemImage: "square.and.arrow.down") {
                                        reviewed = nil
                                        model.beginCredentialImport(workspaceID: workspaceID, credentialID: key.id)
                                    }.disabled(revoked || pending || model.credentialImport != nil)
                                        .accessibilityIdentifier("keys.import.\(key.id)")
                                    Button(pending ? "Retry Saving Revocation" : "Revoke on This Phone", role: .destructive) {
                                        reviewed = model.reviewCredentialRevocation(workspaceID: workspaceID, credentialID: key.id)
                                    }.disabled(revoked && !pending)
                                }.padding(.vertical, 6)
                            }
                        } header: {
                            Text(verbatim: document.name)
                        } footer: {
                            Text("Revocation blocks this fingerprint in every workspace on this phone, including later imports. It does not erase stored key material or revoke access on your Mac or SSH servers.")
                        }
                    }.disabled(model.busy)
                } else {
                    ContentUnavailableView("Workspace Unavailable", systemImage: "key",
                        description: Text("Open a saved workspace to manage its key references."))
                }
            }
            .navigationTitle("SSH Keys").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }.disabled(model.busy)
                }
            }
        }
        .interactiveDismissDisabled(model.busy)
        .sheet(item: Binding(get: { importing }, set: { if $0 == nil { importing?.cancel() } })) { controller in
            WorkspaceKeyImportSheet(controller: controller)
        }
        .confirmationDialog("Revoke Key on This Phone?",
                            isPresented: Binding(get: { reviewed != nil }, set: { if !$0 { reviewed = nil } }),
                            titleVisibility: .visible, presenting: reviewed) { review in
            Button("Revoke and Disconnect", role: .destructive) { model.revokeCredentialLocally(review) }
                .disabled(model.busy || !model.active)
            Button("Keep Key", role: .cancel) { reviewed = nil }
        } message: { review in
            Text(verbatim: review.algorithm + "\n" + review.fingerprint
                + "\n\nBlock every reference to this key on this phone. All sessions in affected workspaces will disconnect; remote shells and programs may end, and input already sent may have executed.\n\n"
                + review.workspaceNames.joined(separator: "\n")
                + "\n\nFuture imports cannot restore permission. Your Mac and SSH servers are unchanged.")
        }
        .onChange(of: model.active) { _, active in if !active { reviewed = nil } }
        .onDisappear { reviewed = nil }
    }
}

@MainActor
private struct MachineWorkspaceView: View {
    let initialDocument: WorkspaceDocument
    let workspaceID: UUID
    @Bindable var model: MobileWorkspaceModel
    /// A relaunch's return: this machine's terminal opens, as Connect would.
    var returningMachineID: String? = nil
    @State private var returned: String?
    @State private var returnHandled = false
    @State private var selection = ""
    @State private var groupID = ""
    @State private var themeMachineID: String?
    @State private var showKeys = false
    /// Read so a return from Tailscale re-evaluates what Connect needs.
    @Environment(\.scenePhase) private var scenePhase
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
                    VStack(alignment: .leading, spacing: 20) {
                        HStack(alignment: .firstTextBaseline) {
                            Text(verbatim: machine.name).font(.largeTitle.bold()).lineLimit(2)
                            Spacer(minLength: 8)
                            if MobileWorkspaceModel.themeCatalog != nil {
                                Button { themeMachineID = machine.id } label: {
                                    Label(theme?.name ?? "Theme", systemImage: "paintpalette").lineLimit(1)
                                }.buttonStyle(.bordered).controlSize(.small).tint(Color(ftHex: theme?.accent ?? "#3B82F6"))
                                    .accessibilityIdentifier("machine.theme")
                            }
                        }
                        HStack(spacing: 0) { ForEach(0..<16) { index in Color(ftHex: theme?.ansi[index] ?? "#000000") } }
                            .frame(height: 10).clipShape(Capsule()).accessibilityHidden(true)
                        connectSection(machine, theme: theme)
                        DisclosureGroup("Routes") {
                            VStack(alignment: .leading, spacing: 14) {
                                ForEach(machine.routeIDs, id: \.self) { id in
                                    if let route = document.routes.first(where: { $0.id == id }) { routeRow(route, preferred: id == machine.preferredRouteID) }
                                }
                            }.padding(.top, 10)
                        }.tint(Color(ftHex: theme?.foreground ?? "#FFFFFF"))
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
                    }.padding(28).padding(.bottom, 24).frame(maxWidth: .infinity, alignment: .leading)
                }.frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .leading)
                    .foregroundStyle(Color(ftHex: theme?.foreground ?? "#FFFFFF"))
                    .background(Color(ftHex: theme?.background ?? "#10223C"))
                    .tag(machine.id)
            }
        }.tabViewStyle(.page(indexDisplayMode: .always))
            .sheet(isPresented: $showKeys) { WorkspaceKeysSheet(workspaceID: workspaceID, model: model) }
            .sheet(isPresented: Binding(get: { themeMachineID != nil }, set: { if !$0 { themeMachineID = nil } })) {
                if let id = themeMachineID, let machine = document.machines.first(where: { $0.id == id }),
                   let themes = model.machineThemes(workspaceID, document: document) {
                    MachineThemePicker(machine: machine, themes: themes) { choice in
                        model.chooseTheme(choice, machineID: id, workspaceID: workspaceID)
                    }
                }
            }
            .navigationTitle(document.name).navigationBarTitleDisplayMode(.inline)
            .navigationDestination(item: $returned) { id in
                if let connections = model.connections, let local = model.localPolicies[workspaceID] {
                    liveWorkspace(connections: connections, local: local, initial: id)
                }
            }
            .onAppear {
                if selection.isEmpty {
                    selection = machines.first(where: { $0.id == returningMachineID })?.id ?? document.initialMachineID
                }
                returnToTerminal()
            }
            .onChange(of: model.localPolicies[workspaceID]?.workspace.revision) { _, _ in
                if !document.groups.contains(where: { $0.id == groupID }) { groupID = document.initialGroupID }
                if !machines.contains(where: { $0.id == selection }) { selection = machines.first?.id ?? document.initialMachineID }
                returnToTerminal()
            }
            .toolbar {
                if !document.credentials.isEmpty {
                    Button("Keys", systemImage: "key") { showKeys = true }.disabled(model.busy)
                }
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

    /// One Connect for the machine, in its colors, and how it will go: the
    /// route this phone can reach now, or what it needs first.
    @ViewBuilder private func connectSection(_ machine: WorkspaceDocument.Machine, theme: WorkspaceDocument.Theme?) -> some View {
        let _ = scenePhase
        let accent = Color(ftHex: theme?.accent ?? "#3B82F6")
        if document.credentials.contains(where: { model.unconfirmedCredentialRevocations.contains($0.fingerprint) }) {
            Label("Finish saving the key revocation in Keys before connecting.", systemImage: "exclamationmark.shield")
                .font(.callout)
        } else if let connections = model.connections, let local = model.localPolicies[workspaceID],
           let target = sceneMachine(machine, local: local).target, let first = target.attemptOrder().first {
            VStack(alignment: .leading, spacing: 10) {
                NavigationLink {
                    liveWorkspace(connections: connections, local: local, initial: machine.id)
                } label: {
                    // A session that is already running opens where it is.
                    let live = model.liveStatus(workspaceID, machineID: machine.id) == .live
                    Label(live ? "Open Terminal" : "Connect", systemImage: "terminal.fill").font(.title3.weight(.semibold))
                        .frame(maxWidth: .infinity).padding(.vertical, 6)
                }.buttonStyle(.borderedProminent).controlSize(.large).tint(accent).disabled(model.busy)
                // Tailscale only: say so before the first attempt waits out a timeout.
                if target.routes.allSatisfy({ WorkspaceRouteNetwork(host: $0.networkHost) == .tailnet }),
                   WorkspaceRouteNetwork.deviceTailnetAddress() == nil {
                    HStack {
                        Label("Needs Tailscale on this \(UIDevice.current.model)", systemImage: "network.badge.shield.half.filled")
                            .font(.callout)
                        Spacer(minLength: 8)
                        Button("Open Tailscale") { WorkspaceTerminalScreen.openTailscale() }.buttonStyle(.bordered).tint(accent)
                    }
                } else {
                    Text(verbatim: WorkspaceTerminalScreen.via(first)).font(.callout).opacity(0.75)
                }
            }
        } else if model.localPolicies[workspaceID] == nil {
            Label("Unlock protected storage to connect.", systemImage: "lock.shield").font(.callout)
        } else {
            Label("No route to this machine is ready on this \(UIDevice.current.model). Routes below say what each needs.",
                  systemImage: "exclamationmark.triangle").font(.callout)
        }
    }

    /// Opens the returning machine's terminal once, when Connect would be
    /// offered for it; until then this page shows what it still needs.
    private func returnToTerminal() {
        guard !returnHandled, let id = returningMachineID else { return }
        guard let machine = document.machines.first(where: { $0.id == id }) else { returnHandled = true; return }
        guard !document.credentials.contains(where: { model.unconfirmedCredentialRevocations.contains($0.fingerprint) }),
              model.connections != nil, let local = model.localPolicies[workspaceID],
              sceneMachine(machine, local: local).target?.attemptOrder().first != nil else { return }
        returnHandled = true
        returned = id
    }

    /// A saved route: where it goes and anything this phone still needs for it.
    @ViewBuilder private func routeRow(_ route: WorkspaceDocument.Route, preferred: Bool) -> some View {
        let hop = route.jumpRoutes.first.flatMap { id in document.routes.first(where: { $0.id == id }) }
        let network = WorkspaceRouteNetwork(host: hop?.host ?? route.host)
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Image(systemName: network == .tailnet ? "network.badge.shield.half.filled" : network == .localNetwork ? "wifi" : "globe")
                Text(verbatim: "\(route.user)@\(route.host):\(route.port)").font(.system(.callout, design: .monospaced))
                    .lineLimit(1).truncationMode(.middle).textSelection(.enabled)
                if preferred { Text("Preferred").font(.caption2.bold()).padding(.horizontal, 6).padding(.vertical, 2).background(.white.opacity(0.15), in: Capsule()) }
            }
            if let hop, route.jumpRoutes.count == 1, hop.jumpRoutes.isEmpty {
                Text(verbatim: "Through \(hop.user)@\(hop.host):\(hop.port)")
                    .font(.system(.caption, design: .monospaced)).opacity(0.75).textSelection(.enabled)
            } else if !route.jumpRoutes.isEmpty {
                Text("This jump-host chain needs more than the one hop supported by this build.").font(.caption).opacity(0.75)
            }
            if route.mode == "frankenterm_mux" {
                Text("Attaches to this machine's existing FrankenTerm panes, like the desktop app.").font(.caption).opacity(0.75)
                // The exact command this route runs on the machine to reach its mux.
                Text(verbatim: route.remoteCommand ?? WorkspaceNativeSSHConnection.defaultMuxCommand)
                    .font(.system(.caption2, design: .monospaced)).opacity(0.6)
                    .lineLimit(2).truncationMode(.middle).textSelection(.enabled)
                    .accessibilityLabel("Runs \(route.remoteCommand ?? WorkspaceNativeSSHConnection.defaultMuxCommand)")
            }
            ForEach(Array((model.localPolicies[workspaceID]?.routes.first { $0.routeID == route.id }?.issues ?? []).enumerated()),
                    id: \.offset) { _, issue in
                Label(issue.message, systemImage: "exclamationmark.circle").font(.caption)
            }
        }
    }

    /// The workspace's live scene, kept by the app across visits, opening on
    /// `initial`. Every machine remains navigable, while only this group's
    /// opted-in peers prepare authenticated connections in the background.
    private func liveWorkspace(connections: MobileConnections, local: WorkspaceLocalState, initial: String) -> some View {
        let themes = SceneThemeSource(
            themes: { [model, workspaceID, document] in model.machineThemes(workspaceID, document: document) },
            choose: { [model, workspaceID] machineID, choice in model.chooseTheme(choice, machineID: machineID, workspaceID: workspaceID) })
        let ordered = machines + document.machines.filter { machine in !machines.contains { $0.id == machine.id } }
        let selectedGroupID = groupID.isEmpty ? document.initialGroupID : groupID
        let group = document.groups.first(where: { $0.id == selectedGroupID }).map {
            TerminalConnectionIntent.Group(id: $0.id, machineIDs: $0.machineOrder, autoConnect: $0.autoConnect)
        }
        return LiveWorkspaceView(initialMachineID: initial, themes: themes, group: group, connectOnOpen: true,
                                 onShowing: { [model, workspaceID] machineID in
            model.showing(machineID.map { .init(workspaceID: workspaceID, machineID: $0) })
        }) { [model, workspaceID, document] in
            try model.liveScene(workspaceID: workspaceID, revision: local.workspace.revision) {
                try WorkspaceTerminalScene(workspaceID: workspaceID, revision: local.workspace.revision,
                    workspaceName: document.name, machines: ordered.map { sceneMachine($0, local: local) },
                    initialMachineID: initial, connections: connections, initialGroup: group)
            }
        }
    }

    private func sceneMachine(_ machine: WorkspaceDocument.Machine, local: WorkspaceLocalState) -> SceneMachine {
        let theme = theme(for: machine)
        func scene(_ target: TerminalSessionTarget?) -> SceneMachine {
            .init(id: machine.id, name: machine.name, target: target,
                  unavailable: "None of this machine's SSH routes is ready on this device. Open the machine from the workspace to see what each route needs.",
                  theme: theme, autoConnect: machine.autoConnect)
        }
        // Every ready route, the preferred one first: mux routes attach to the
        // machine's existing panes, direct-SSH routes to its mux if one answers.
        var seen = Set<String>(), routes: [TerminalSessionRoute] = []
        for id in [machine.preferredRouteID] + machine.routeIDs where seen.insert(id).inserted {
            guard let route = document.routes.first(where: { $0.id == id }),
                  route.mode == "direct_ssh" || route.mode == "frankenterm_mux",
                  route.jumpRoutes.count <= 1, let readiness = local.routes.first(where: { $0.routeID == id }),
                  readiness.issues.isEmpty, !readiness.usableCredentialIDs.isEmpty else { continue }
            let hop = route.jumpRoutes.first.flatMap { hopID in document.routes.first(where: { $0.id == hopID }) }
            guard route.jumpRoutes.isEmpty || hop?.jumpRoutes.isEmpty == true else { continue }
            routes.append(.init(id: id, credentialIDs: readiness.usableCredentialIDs, host: route.host, port: route.port,
                                mux: route.mode == "frankenterm_mux",
                                sessionCreation: route.mode == "frankenterm_mux" ? .existingOnly
                                    : (route.sessionCreation == "manual" ? .manual : .onSelect),
                                jumpHost: hop?.host, jumpPort: hop?.port))
        }
        guard !routes.isEmpty else { return scene(nil) }
        return scene(.init(workspaceID: workspaceID, revision: local.workspace.revision, machineID: machine.id,
                           machineName: machine.name, routes: routes, theme: theme))
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
