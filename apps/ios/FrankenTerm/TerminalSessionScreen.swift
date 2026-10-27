import SwiftUI
import UIKit
import Observation
import os
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration
import FrankenTermTerminal

/// One visible, app-owned password editor. It is scoped to the terminal the
/// person is looking at, and can never store or automatically reuse a password.
@MainActor
@Observable
final class MobileSSHPasswordPrompt: WorkspaceSSHPasswordPrompting {
    struct Scope: Equatable {
        let workspaceID: UUID, revision: UUID
        let machineID: String
        let destinationRouteIDs: Set<String>
    }
    private(set) var request: WorkspaceSSHPasswordRequest?
    private(set) var secret = ""
    @ObservationIgnored private var active = false
    @ObservationIgnored private var scope: Scope?
    @ObservationIgnored private var continuation: CheckedContinuation<Data, Error>?
    var isActive: Bool { active }

    func setActive(_ value: Bool) {
        active = value
        if !value { cancelCurrent() }
    }
    func setScope(_ value: Scope?) {
        if scope != value { cancelCurrent() }
        scope = value
    }
    private func permits(_ request: WorkspaceSSHPasswordRequest) -> Bool {
        guard active, let scope else { return false }
        let authorization = request.authorization
        return scope.workspaceID == authorization.workspaceID && scope.revision == authorization.workspaceRevision
            && scope.destinationRouteIDs.contains(authorization.transitForRouteID ?? authorization.routeID)
    }
    func password(for request: WorkspaceSSHPasswordRequest) async throws -> Data {
        try Task.checkCancellation()
        guard permits(request) else { throw SSHAuthenticationError.stale }
        guard self.request == nil, continuation == nil else { throw SSHAuthenticationError.busy }
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                guard !Task.isCancelled, permits(request) else {
                    continuation.resume(throwing: CancellationError()); return
                }
                secret = ""; self.request = request; self.continuation = continuation
            }
        } onCancel: { self.cancel(requestID: request.id) }
    }
    nonisolated func cancel(requestID: UUID) {
        Task { @MainActor in self.cancelNow(requestID: requestID) }
    }
    func cancelNow(requestID: UUID) {
        guard request?.id == requestID else { return }
        cancelCurrent()
    }
    private func cancelCurrent() {
        let pending = continuation
        continuation = nil; secret = ""; request = nil
        pending?.resume(throwing: CancellationError())
    }
    var canSubmit: Bool { (1...1024).contains(secret.utf8.count) && !secret.utf8.contains(0) }
    func secretValue(requestID: UUID) -> String { request?.id == requestID ? secret : "" }
    func editSecret(_ value: String, requestID: UUID) {
        guard request?.id == requestID, value.utf8.count <= 1024, !value.utf8.contains(0) else { return }
        secret = value
    }
    func submit(requestID: UUID) {
        guard let request, request.id == requestID, permits(request), canSubmit, let continuation else { return }
        let bytes = Data(secret.utf8)
        self.continuation = nil; secret = ""; self.request = nil
        continuation.resume(returning: bytes)
    }
}

@MainActor
struct MobileSSHPasswordSheet: View {
    @Bindable var prompt: MobileSSHPasswordPrompt
    let request: WorkspaceSSHPasswordRequest
    @FocusState private var focused: Bool
    var body: some View {
        NavigationStack {
            Form {
                Section(request.authorization.transitForRouteID == nil ? "Verified machine" : "Verified jump host") {
                    Text(verbatim: "\(request.authorization.user)@\(request.authorization.host):\(request.authorization.port)")
                        .font(.system(.body, design: .monospaced)).textSelection(.enabled)
                    Text(verbatim: request.authorization.presentedHostKey.fingerprint)
                        .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                }
                Section {
                    SecureField("SSH password", text: Binding(get: { prompt.secretValue(requestID: request.id) },
                        set: { prompt.editSecret($0, requestID: request.id) }))
                        .textContentType(.password).textInputAutocapitalization(.never).autocorrectionDisabled()
                        .focused($focused).submitLabel(.go)
                        .onSubmit { prompt.submit(requestID: request.id) }
                        .accessibilityIdentifier("ssh.password.field")
                } footer: {
                    Text("The host key matches the workspace saved on this phone. This password is used once and is not saved. This prompt does not change the account's password.")
                }
            }
            .navigationTitle("SSH Password").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { prompt.cancelNow(requestID: request.id) }
                        .accessibilityIdentifier("ssh.password.cancel")
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Sign In") { prompt.submit(requestID: request.id) }
                        .disabled(!prompt.canSubmit || prompt.request?.id != request.id)
                        .accessibilityIdentifier("ssh.password.submit")
                }
            }
        }
        .interactiveDismissDisabled()
        .task { focused = true }
        .onDisappear { prompt.cancelNow(requestID: request.id) }
    }
}

/// One foreground editor for an exact keyboard-interactive round. Echo policy
/// comes from the bounded server challenge; no response is saved or reused.
@MainActor
@Observable
final class MobileSSHKeyboardInteractivePrompt: WorkspaceSSHKeyboardInteractivePrompting {
    private(set) var request: WorkspaceSSHKeyboardInteractiveRequest?
    private(set) var answers: [String] = []
    @ObservationIgnored private var active = false
    @ObservationIgnored private var scope: MobileSSHPasswordPrompt.Scope?
    @ObservationIgnored private var continuation: CheckedContinuation<[Data], Error>?

    func setActive(_ value: Bool) {
        active = value
        if !value { cancelCurrent() }
    }
    func setScope(_ value: MobileSSHPasswordPrompt.Scope?) {
        if scope != value { cancelCurrent() }
        scope = value
    }
    private func permits(_ request: WorkspaceSSHKeyboardInteractiveRequest) -> Bool {
        guard active, let scope else { return false }
        let authorization = request.authorization
        return scope.workspaceID == authorization.workspaceID && scope.revision == authorization.workspaceRevision
            && scope.destinationRouteIDs.contains(authorization.transitForRouteID ?? authorization.routeID)
    }
    func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) async throws -> [Data] {
        try Task.checkCancellation()
        guard permits(request) else { throw SSHAuthenticationError.stale }
        guard self.request == nil, continuation == nil else { throw SSHAuthenticationError.busy }
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                guard !Task.isCancelled, permits(request) else {
                    continuation.resume(throwing: CancellationError()); return
                }
                answers = Array(repeating: "", count: request.challenge.prompts.count)
                self.request = request; self.continuation = continuation
            }
        } onCancel: { self.cancel(requestID: request.id) }
    }
    nonisolated func cancel(requestID: UUID) {
        Task { @MainActor in self.cancelNow(requestID: requestID) }
    }
    func cancelNow(requestID: UUID) {
        guard request?.id == requestID else { return }
        cancelCurrent()
    }
    private func cancelCurrent() {
        let pending = continuation
        continuation = nil; answers = []; request = nil
        pending?.resume(throwing: CancellationError())
    }
    func answer(at index: Int, requestID: UUID) -> String {
        guard request?.id == requestID, answers.indices.contains(index) else { return "" }
        return answers[index]
    }
    func editAnswer(_ value: String, index: Int, requestID: UUID) {
        guard request?.id == requestID, answers.indices.contains(index), value.utf8.count <= SSHKeyboardInteractiveChallenge.maximumResponseBytes,
              !value.utf8.contains(0) else { return }
        answers[index] = value
    }
    var canSubmit: Bool {
        guard let request, answers.count == request.challenge.prompts.count else { return false }
        return answers.allSatisfy { $0.utf8.count <= SSHKeyboardInteractiveChallenge.maximumResponseBytes && !$0.utf8.contains(0) }
    }
    func submit(requestID: UUID) {
        guard let request, request.id == requestID, permits(request), canSubmit, let continuation else { return }
        let bytes = answers.map { Data($0.utf8) }
        self.continuation = nil; answers = []; self.request = nil
        continuation.resume(returning: bytes)
    }
}

@MainActor
struct MobileSSHKeyboardInteractiveSheet: View {
    @Bindable var prompt: MobileSSHKeyboardInteractivePrompt
    let request: WorkspaceSSHKeyboardInteractiveRequest
    @FocusState private var focused: Int?

    private func answer(_ index: Int) -> Binding<String> {
        Binding(get: { prompt.answer(at: index, requestID: request.id) },
                set: { prompt.editAnswer($0, index: index, requestID: request.id) })
    }

    var body: some View {
        NavigationStack {
            Form {
                Section(request.authorization.transitForRouteID == nil ? "Verified machine" : "Verified jump host") {
                    Text(verbatim: "\(request.authorization.user)@\(request.authorization.host):\(request.authorization.port)")
                        .font(.system(.body, design: .monospaced)).textSelection(.enabled)
                    Text(verbatim: request.authorization.presentedHostKey.fingerprint)
                        .font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                }
                Section("Server challenge · Round \(request.challenge.round)") {
                    if !request.challenge.name.isEmpty { Text(verbatim: request.challenge.name) }
                    if !request.challenge.instruction.isEmpty { Text(verbatim: request.challenge.instruction) }
                    ForEach(request.challenge.prompts.indices, id: \.self) { index in
                        let field = request.challenge.prompts[index]
                        VStack(alignment: .leading, spacing: 8) {
                            Text(verbatim: field.text.isEmpty ? "Response \(index + 1)" : field.text)
                            Group {
                                if field.echo {
                                    TextField("Response", text: answer(index))
                                } else {
                                    SecureField("Response", text: answer(index))
                                }
                            }
                            .textInputAutocapitalization(.never).autocorrectionDisabled()
                            .focused($focused, equals: index).submitLabel(.continue)
                            .onSubmit {
                                if index + 1 < request.challenge.prompts.count { focused = index + 1 }
                                else { prompt.submit(requestID: request.id) }
                            }
                            .accessibilityLabel(field.text.isEmpty ? "Response \(index + 1)" : field.text)
                            .accessibilityIdentifier("ssh.interactive.response.\(index)")
                        }
                    }
                } footer: {
                    Text("The challenge text comes from this verified server. Responses are sent only for this sign-in and are not saved. Continue also acknowledges an informational round with no fields.")
                }
            }
            .navigationTitle("SSH Verification").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { prompt.cancelNow(requestID: request.id) }
                        .accessibilityIdentifier("ssh.interactive.cancel")
                }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Continue") { prompt.submit(requestID: request.id) }
                        .disabled(!prompt.canSubmit || prompt.request?.id != request.id)
                        .accessibilityIdentifier("ssh.interactive.submit")
                }
            }
        }
        .interactiveDismissDisabled()
        .task { focused = request.challenge.prompts.isEmpty ? nil : 0 }
        .onDisappear { prompt.cancelNow(requestID: request.id) }
    }
}

enum MobileSSHAuthenticationSheetRequest: Identifiable {
    case password(WorkspaceSSHPasswordRequest)
    case keyboardInteractive(WorkspaceSSHKeyboardInteractiveRequest)
    var id: UUID {
        switch self {
        case let .password(request): request.id
        case let .keyboardInteractive(request): request.id
        }
    }
}

/// App-wide live-connection services over the app's single WorkspaceStore: one
/// input coordinator, one durable shell-creation journal and one connection
/// manager (which owns the authentication broker's foreground state).
@MainActor
final class MobileConnections {
    let coordinator: TerminalInputCoordinator
    let manager: WorkspaceSSHConnectionManager
    /// Durable record of sessions whose input may have unknown remote effects.
    let journal: TerminalSessionJournal
    let passwordPrompt = MobileSSHPasswordPrompt()
    let keyboardInteractivePrompt = MobileSSHKeyboardInteractivePrompt()
    var authenticationPrompt: MobileSSHAuthenticationSheetRequest? {
        if let request = passwordPrompt.request { return .password(request) }
        if let request = keyboardInteractivePrompt.request { return .keyboardInteractive(request) }
        return nil
    }
    private let broker: SSHAuthenticationBroker
    private var passwordLifecycle: Task<Void, Never>?
    // Failed cleanup retains its existing bounded connection/model owner even
    // after SwiftUI removes the screen. Never manufacture a replacement owner
    // or forget an unresolved protected-storage write to make room for a shell.
    private var pendingCleanup: [UUID: TerminalSessionController] = [:]

    /// `signer` defaults to the protected Keychain signer; tests inject one,
    /// and may shorten how long an unanswering server takes to be reported.
    init(store: WorkspaceStore, journalDirectory: URL,
         journalIdentifier: String = "primary",
         signer: any WorkspaceSSHSigning = AppleSSHCredentialSigner(),
         liveness: WorkspaceSSHLiveness = .standard) throws {
        let broker = SSHAuthenticationBroker(store: store, signer: signer,
                                             support: WorkspaceNativeSSHAuthentication.routeSupport)
        self.broker = broker
        coordinator = try TerminalInputCoordinator()
        journal = try TerminalSessionJournal(directory: journalDirectory, identifier: journalIdentifier)
        // Machines with a FrankenTerm mux are attached to like the desktop app
        // does: the same panes, through the Rust mux client.
        manager = WorkspaceSSHConnectionManager(store: store, broker: broker, journal: journal,
                                                coordinator: coordinator, liveness: liveness,
                                                muxClientFactory: { try NativeMuxClient() }, passwordPrompter: passwordPrompt,
                                                keyboardInteractivePrompter: keyboardInteractivePrompt)
    }

    func setAuthenticationForeground(_ active: Bool) {
        passwordPrompt.setActive(active)
        keyboardInteractivePrompt.setActive(active)
        let prior = passwordLifecycle, broker = broker
        passwordLifecycle = Task {
            await prior?.value
            await broker.setPasswordForeground(active)
        }
    }

    func setAuthenticationScope(_ scope: MobileSSHPasswordPrompt.Scope?) {
        passwordPrompt.setScope(scope)
        keyboardInteractivePrompt.setScope(scope)
    }

    /// Admits connections and retries any cleanup that failed earlier. Sessions
    /// run until the user closes them: leaving the foreground does not suspend
    /// the manager (a dropped connection reconnects on return instead).
    func activate() async {
        for controller in Array(pendingCleanup.values) { await controller.retryCleanup() }
        await manager.resume()
        await passwordLifecycle?.value
        await broker.setPasswordForeground(passwordPrompt.isActive)
    }

    fileprivate func retainForCleanup(_ controller: TerminalSessionController) {
        pendingCleanup[controller.id] = controller
    }
    fileprivate func releaseCleanup(_ id: UUID) { pendingCleanup[id] = nil }
}

/// One ready route, with its available configured keys in the workspace's
/// order. A mux route attaches to existing FrankenTerm panes rather than
/// starting a shell. Readiness never authorizes unrelated Keychain identities.
struct TerminalSessionRoute: Hashable, Sendable {
    let id: String, credentialIDs: [String], host: String, port: UInt16
    var authenticationChoices: [WorkspaceSSHAuthenticationChoice]? = nil
    var promptingJump = false
    var mux = false
    var sessionCreation: TerminalSessionCreation = .onSelect
    var jumpHost: String? = nil
    var jumpPort: UInt16? = nil

    /// Only the first SSH endpoint must be reachable from this device. The
    /// destination's private address is resolved and reached by its jump host.
    var networkHost: String { jumpHost ?? host }
    var networkPort: UInt16 { jumpHost == nil ? port : (jumpPort ?? 22) }
    var choices: [WorkspaceSSHAuthenticationChoice] {
        authenticationChoices ?? credentialIDs.map(WorkspaceSSHAuthenticationChoice.publicKey)
    }
    var requiresAuthenticationPrompt: Bool { promptingJump || choices.contains(where: \.requiresForegroundPrompt) }
}

/// A machine to open and its ready routes, the chosen or preferred one first.
struct TerminalSessionTarget: Hashable, Sendable {
    let workspaceID: UUID, revision: UUID
    let machineID: String, machineName: String
    /// Never empty.
    let routes: [TerminalSessionRoute]
    /// The machine theme as the terminal's base palette, and its background.
    let basePalette: [UInt8]?, background: String?
    var requiresAuthenticationPrompt: Bool { routes.contains(where: \.requiresAuthenticationPrompt) }

    init(workspaceID: UUID, revision: UUID, machineID: String, machineName: String,
         routes: [TerminalSessionRoute], theme: WorkspaceDocument.Theme? = nil) {
        precondition(!routes.isEmpty, "a session target needs a route")
        self.workspaceID = workspaceID; self.revision = revision; self.machineID = machineID
        self.machineName = machineName; self.routes = routes
        basePalette = theme?.terminalBasePalette; background = theme?.background
    }

    /// The same routes under another theme.
    func wearing(_ theme: WorkspaceDocument.Theme) -> TerminalSessionTarget {
        .init(workspaceID: workspaceID, revision: revision, machineID: machineID, machineName: machineName,
              routes: routes, theme: theme)
    }

    /// The order to try the routes in: first hops this device can reach now first
    /// (see WorkspaceRouteNetwork.isLikelyReachable), each group in the
    /// machine's own order.
    func attemptOrder(isLikelyReachable: (String) -> Bool = WorkspaceRouteNetwork.isLikelyReachable) -> [TerminalSessionRoute] {
        let reachable = routes.map { isLikelyReachable($0.networkHost) }
        return routes.indices.filter { reachable[$0] }.map { routes[$0] }
            + routes.indices.filter { !reachable[$0] }.map { routes[$0] }
    }

    /// Whether light chrome reads better over this background: its relative
    /// luminance is below 0.179, where black and white text contrast equally.
    /// No theme means the black default.
    static func isDark(_ hex: String?) -> Bool {
        guard let hex, let value = UInt32(hex.dropFirst().prefix(6), radix: 16) else { return true }
        func linear(_ shift: UInt32) -> Double {
            let component = Double(value >> shift & 0xFF) / 255
            return component <= 0.04045 ? component / 12.92 : pow((component + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * linear(16) + 0.7152 * linear(8) + 0.0722 * linear(0) < 0.179
    }
}

/// Publishes each owned FTSF frame from the Rust model to the native view.
@MainActor
private final class TerminalFramePresenter: TerminalModelDelivery {
    let source: TerminalSource
    private let connection: WorkspaceSSHConnection
    weak var view: NativeTerminalView?
    weak var clipboard: NativeTerminalClipboardController?
    // The scene fences delivery synchronously, before the model actor applies
    // its hide. An already-captured obsolete frame cannot restart hidden work.
    var acceptsFrames = false
    var onUnavailable: ((TerminalRenderError) -> Void)?
    var onInterruptedInput: (([TerminalInputOutcome]) -> Void)?
    init(source: TerminalSource, connection: WorkspaceSSHConnection) {
        self.source = source; self.connection = connection
    }

    func present(_ presentation: TerminalModelPresentation, source: TerminalSource) async throws {
        guard source == self.source else { throw TerminalInputError.stale }
        guard acceptsFrames else { return }
        switch presentation {
        case let .frame(bytes):
            try view?.submit(bytes, source: source)
            try clipboard?.receiveFrame(bytes, source: source)
        case let .unavailable(reason): onUnavailable?(reason)
        }
    }
    /// Metadata-only, idempotent acceptance of normal AND final outcome batches.
    /// No input bytes or unbounded receipt history survive here. The durable
    /// journal's conservative uncertainty is not resolved by local acceptance.
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {
        guard source == self.source, outcomes.count <= 64,
              outcomes.allSatisfy({ $0.receipt.source == source }) else { throw TerminalInputError.stale }
        // The exact journal epoch outlives native close. Publish content-free
        // evidence before the pump can acknowledge this batch, including final
        // cleanup outcomes delivered after the view has retired.
        try await connection.recordInput(outcomes, source: source)
        if outcomes.contains(where: {
            switch $0.disposition {
            case .connectionLost, .definitelyNotWritten, .cancelledAfterPrefix: true
            case .transportAccepted, .cancelledBeforeWrite: false
            }
        }) {
            onInterruptedInput?(outcomes)
        }
    }
}

/// The workspace scene owns its one keyboard and all navigation bindings.
/// Detachment must succeed before the source and its canonical model retire.
@MainActor
protocol TerminalSessionFocus: AnyObject {
    /// Existing sessions may keep running while iOS permits it; starting a
    /// remote terminal requires the owning scene's current foreground admission.
    var permitsTerminalCreation: Bool { get }
    /// Whether this machine is the one on screen now; a session connecting in
    /// the background waits to attach its view until it is.
    func shows(machineID: String) -> Bool
    /// A terminal on this machine ended or was lost: only New Session may
    /// start another shell there.
    func requiresNewSession(_ machineID: String) -> Bool
    func attach(machineID: String, source: TerminalSource,
                encoder: any TerminalKeyboardEncoding) async throws -> TerminalKeyboardSession
    func detach(machineID: String, source: TerminalSource) async throws
    func inputRetired(machineID: String)
    /// A tap on the terminal asked for the keyboard back.
    func activateKeyboard(showKeyboard: Bool, source: TerminalSource?) async
    /// The user picked a text size on one terminal; every terminal follows.
    func fontSizeChanged(_ size: Double)
}

extension TerminalSessionFocus {
    func activateKeyboard() async { await activateKeyboard(showKeyboard: true, source: nil) }
}

/// Owns one direct-SSH connection, PTY, model and native display. Keyboard
/// generations come from the workspace scene's existing navigation owner.
@MainActor
@Observable
final class TerminalSessionController {
    enum Phase: Equatable {
        case idle, connecting, ready, live
        /// No route reached a signed-in connection, so no shell started and
        /// nothing was sent.
        case couldNotConnect(WorkspaceConnectionHelp)
        case ended(String)
        case failed(String)
    }
    let id = UUID()
    private(set) var phase: Phase = .idle { didSet { if phase != oldValue { onChange?() } } }
    /// The route being tried, then the one connected.
    private(set) var route: TerminalSessionRoute
    private(set) var credentialID: String?
    private(set) var terminal: NativeTerminalView?
    /// True only after every owned cleanup operation actually succeeds.
    private(set) var settled = false { didSet { if settled != oldValue { onChange?() } } }
    /// The connection went away underneath a live session (lost, failed, or
    /// the server stopped answering): worth reconnecting. A remote exit or a
    /// close on this phone is not a drop. A retry of a drop that cannot reach
    /// the machine is still one: the network may be back for the next try.
    private(set) var dropped = false
    @ObservationIgnored private let retriesDrop: Bool
    private(set) var cleanupFailure: String?
    /// What an ended connection left known about the person's input. A later
    /// session on this machine keeps it until they dismiss it: a new live
    /// frame says nothing about whether the earlier input arrived.
    private(set) var inputDelivery: TerminalInputDelivery
    /// The same, kept for the next launch if iOS ends this one (plan 11.8).
    @ObservationIgnored private let unansweredInput: TerminalUnansweredInput
    private(set) var interactionProblem: String?
    private(set) var localInputRecovery: TerminalKeyboardRecovery?
    private(set) var resolvingLocalInput = false
    private(set) var historyInspection: TerminalHistoryInspection?
    private(set) var historyReturnBookmark: TerminalHistoryBookmark?
    private(set) var historyReturning = false
    private(set) var retainedOutput: TerminalRetainedOutput?
    private(set) var retainedOutputFailure: String?
    /// The machine's mux panes and the one on screen, when this session is
    /// attached to its FrankenTerm mux rather than a shell of its own.
    private(set) var muxState: TerminalMuxControl.State?
    /// Output arrived that this machine has not had on screen since: the
    /// shell's while it was away, or any of its mux panes'. Read from the
    /// machine strip, which updates while the machine is off screen.
    var hasUnseenOutput: Bool { unseenShellOutput || unreadMuxPanes }
    private var unseenShellOutput = false
    private var unreadMuxPanes = false
    /// The attachment whose notice the screen has shown; each says it once.
    @ObservationIgnored private weak var noticedMux: TerminalMuxControl?
    /// No mux answered on this machine, so this session is a new shell.
    private(set) var shellOnly = false
    /// This session showed the machine's mux panes, so a retry after it
    /// drops attaches to them again rather than waiting for New Session.
    @ObservationIgnored private(set) var attachedToMux = false
    let target: TerminalSessionTarget
    /// The scene's hook for phase and settlement changes: it starts the next
    /// background connection and reconnects dropped sessions from here.
    @ObservationIgnored var onChange: (@MainActor () -> Void)?

    @ObservationIgnored private let connections: MobileConnections
    @ObservationIgnored private var connection: WorkspaceSSHConnection?
    @ObservationIgnored private var pump: TerminalSessionPump?
    @ObservationIgnored private var runtime: TerminalModelRuntime?
    @ObservationIgnored private var unadoptedModel: NativeTerminalModel?
    @ObservationIgnored private var keyboard: TerminalKeyboardSession?
    @ObservationIgnored private var keyboardRecoveryObservation: (keyboard: TerminalKeyboardSession, token: UUID)?
    @ObservationIgnored private var localInputReset: Task<Void, Never>?
    @ObservationIgnored private var pointerFocusActivation: Task<Void, Never>?
    @ObservationIgnored private weak var focus: (any TerminalSessionFocus)?
    @ObservationIgnored private var attached = false
    @ObservationIgnored private var startup: Task<Void, Never>?
    @ObservationIgnored private var startupID: UUID?
    @ObservationIgnored private var terminalRequested = false
    @ObservationIgnored private var explicitTerminalRequested = false
    @ObservationIgnored private var run: Task<TerminalSessionEnd, Error>?
    @ObservationIgnored private var runObserver: Task<Void, Never>?
    @ObservationIgnored private var cleanup: (id: UUID, task: Task<Void, Never>)?
    @ObservationIgnored private var presenter: TerminalFramePresenter?
    @ObservationIgnored private var clipboard: NativeTerminalClipboardController?
    @ObservationIgnored private var presentation: TerminalPresentationVisibility<TerminalSource>?
    @ObservationIgnored private var presentationVisible = true
    @ObservationIgnored private var overviewBlocking = false
    @ObservationIgnored private var resize: TerminalViewportResize<TerminalSource>?
    @ObservationIgnored private var history: TerminalHistorySelection<TerminalSource>?
    @ObservationIgnored private var lastViewportRevision: UInt64 = 0
    @ObservationIgnored private var stopped = false
    /// The palette the terminal should show, and the one its model has.
    @ObservationIgnored private var basePalette: [UInt8]?
    @ObservationIgnored private var appliedPalette: [UInt8]?
    @ObservationIgnored private var paletteChange: Task<Void, Never>?
    /// Font revisions: the renderer's newest, and the newest the model has.
    @ObservationIgnored private var wantedFontRevision: UInt64 = 1
    @ObservationIgnored private var appliedFontRevision: UInt64 = 1
    @ObservationIgnored private var fontRevisionChange: Task<Void, Never>?
    @ObservationIgnored private var historyReturn: Task<Void, Never>?
    @ObservationIgnored private var muxSearchPresented = false
    @ObservationIgnored private var retainedCaptureAttempted = false
    @ObservationIgnored private var retainEndedOutput = true
    @ObservationIgnored private var viewAttachment: Task<Void, Never>?
    @ObservationIgnored private var authenticatedSelection: TerminalRouteAttemptPlan.Selection?
    @ObservationIgnored private var reconnectSelection: TerminalRouteAttemptPlan.Selection?
    /// Bounded public metadata only. A password hop cannot be reconstructed
    /// automatically after destination key refusal; explicit Retry consumes
    /// the remaining plan once, without re-offering already-refused keys.
    @ObservationIgnored private var passwordHopRetryPlan: TerminalRouteAttemptPlan?
    @ObservationIgnored private var passwordMuxRetry = false
    @ObservationIgnored private var paneHandoff: Task<Void, Never>?
    @ObservationIgnored private var requestedMuxPane: Int?
    @ObservationIgnored private var requestedMuxNavigation: TerminalMuxInputRouting.NavigationPlan?
    @ObservationIgnored private var displayedMuxDestination: TerminalMuxInputRouting.Destination?
    /// One joined presentation worker, never one MainActor task per mux event.
    @ObservationIgnored private var muxObservation: TerminalMuxObservation?

    init(target: TerminalSessionTarget, connections: MobileConnections, focus: any TerminalSessionFocus,
         reconnectingFrom previous: TerminalSessionController? = nil,
         retryingAfterPasswordHop retried: TerminalSessionController? = nil,
         inputDelivery: TerminalInputDelivery = .init()) {
        self.target = target; self.connections = connections; self.focus = focus
        self.inputDelivery = inputDelivery
        unansweredInput = TerminalUnansweredInput(workspace: target.workspaceID.uuidString, machine: target.machineID)
        basePalette = target.basePalette
        route = target.routes[0]
        retriesDrop = previous?.dropped == true
        if let previous, previous.target.workspaceID == target.workspaceID,
           previous.target.revision == target.revision, previous.target.machineID == target.machineID,
           let selected = previous.authenticatedSelection ?? previous.reconnectSelection,
           target.routes.contains(where: { $0.id == selected.routeID && $0.choices.contains(selected.authentication) }) {
            reconnectSelection = selected
            shellOnly = previous.shellOnly
        }
        if let retried {
            let pending = retried.passwordHopRetryPlan
            let direct = retried.passwordMuxRetry
            retried.passwordHopRetryPlan = nil; retried.passwordMuxRetry = false
            if retried.target.workspaceID == target.workspaceID, retried.target.revision == target.revision,
               retried.target.machineID == target.machineID, retried.target.routes == target.routes,
               pending != nil || direct {
                passwordHopRetryPlan = pending; passwordMuxRetry = direct
                shellOnly = retried.shellOnly
            }
        }
    }

    /// "Machine · Route", for labels that leave the session.
    private var destination: String { "\(target.machineName) · Machine \(target.machineID) · Route \(route.id)" }

    /// Re-themes the terminal. Before the model exists, startup applies the
    /// latest palette; after, one change at a time reaches the runtime and
    /// the last pick wins.
    func applyPalette(_ palette: [UInt8]) {
        basePalette = palette
        guard paletteChange == nil else { return }
        paletteChange = Task { [weak self] in
            while let self, !self.stopped, let runtime = self.runtime, let source = self.pump?.source,
                  let wanted = self.basePalette, wanted != self.appliedPalette {
                do { try await runtime.setBasePalette(wanted, source: source); self.appliedPalette = wanted }
                catch { break }
            }
            self?.paletteChange = nil
        }
    }

    var clipboardIsPresenting: Bool {
        clipboard?.isPresenting == true || historyInspection != nil || historyReturning || muxSearchPresented
    }
    var localInputNeedsDecision: Bool { localInputRecovery != nil || resolvingLocalInput }
    /// A local command editor blocks input. Its explicit Review transition is
    /// the one exception: it restores this same terminal's keyboard so the
    /// existing native paste review can bind to a fresh focus generation.
    var keyboardActivationBlocked: Bool {
        clipboard?.blocksKeyboardActivation == true || historyInspection != nil || historyReturning || muxSearchPresented
    }

    /// The fleet picker owns the text responder while open. Its machine/pane
    /// navigation may prepare a destination, but no late view build, caret or
    /// responder publication can enable shell input behind that picker.
    func setOverviewBlocking(_ blocked: Bool) {
        guard overviewBlocking != blocked else { return }
        overviewBlocking = blocked
        if blocked {
            pointerFocusActivation?.cancel()
            localInputReset?.cancel()
            viewAttachment?.cancel()
            terminal?.input.retire(); keyboard?.suspend()
            connection?.muxControl?.inputRouting.discardPointers()
        }
    }

    /// A cancelled display build still owns its resources until it settles.
    /// The picker joins it before starting a competing pane/view transition.
    func waitForOverviewPresentation() async {
        if let viewAttachment { await viewAttachment.value }
        if let paneHandoff { await paneHandoff.value }
    }
    func beginMuxSearch() {
        guard !stopped, phase == .live, presentationVisible, !localInputNeedsDecision else { return }
        connection?.muxControl?.inputRouting.discardPointers()
        muxSearchPresented = true
        viewAttachment?.cancel()
        terminal?.input.retire()
    }
    func endMuxSearch(resumingView: Bool = true) {
        muxSearchPresented = false
        connection?.muxControl?.clearSearch()
        if resumingView { showView() }
    }
    func inspectOutput() {
        guard !stopped, phase == .live, presentationVisible, !clipboardIsPresenting, !localInputNeedsDecision else { return }
        connection?.muxControl?.inputRouting.discardPointers()
        interactionProblem = nil; clipboard?.requestCopy()
    }
    func paste() {
        guard !stopped, phase == .live, presentationVisible, !clipboardIsPresenting, !localInputNeedsDecision else { return }
        connection?.muxControl?.inputRouting.discardPointers()
        interactionProblem = nil; clipboard?.requestPaste()
    }
    func composeCommand() {
        guard !stopped, phase == .live, presentationVisible, !clipboardIsPresenting,
              !overviewBlocking, !paneNavigationBusy else { return }
        connection?.muxControl?.inputRouting.discardPointers()
        interactionProblem = nil; clipboard?.requestCommandDraft()
    }

    /// A draft is tied to the exact displayed view/source/mux visit. Returning
    /// from its local editor may replace the retired keyboard, but never switch
    /// machines, revisit a pane, reconnect, or create a remote terminal.
    private func restoreCommandDraftFocus(view: NativeTerminalView, source: TerminalSource,
                                          destination: TerminalMuxInputRouting.Destination?) async throws -> TerminalCommandDraftDestination {
        func checkDestination() throws {
            try Task.checkCancellation()
            guard !stopped, phase == .live, presentationVisible, !overviewBlocking,
                  terminal === view, pump?.source == source, displayedMuxDestination == destination,
                  clipboard?.isPresenting == true, clipboard?.blocksKeyboardActivation == false,
                  paneHandoff == nil, requestedMuxPane == nil,
                  focus?.shows(machineID: target.machineID) == true,
                  connection?.muxControl?.inputRouting.current.destination == destination,
                  connection?.muxControl?.inputRouting.current.ready != false else {
                throw CancellationError()
            }
        }
        try checkDestination()
        guard let focus else { throw TerminalCommandDraftError.unavailable }
        await focus.activateKeyboard()
        try checkDestination()
        guard view.input.session.originalFocus.source == source,
              view.input.canReviewPaste else {
            throw TerminalCommandDraftError.unavailable
        }
        return TerminalCommandDraftDestination(source: source,
            mux: connection?.muxControl?.inputRouting.current.destination)
    }
    func dismissInteractionProblem() { interactionProblem = nil }

    /// The owner is captured by the sheet, never looked up again through a
    /// possibly different selected machine when that sheet is dismissed.
    func inspectHistory() -> TerminalHistoryInspection? {
        guard !stopped, phase == .live, presentationVisible, !clipboardIsPresenting, !localInputNeedsDecision,
              let runtime, let source = pump?.source else { return nil }
        connection?.muxControl?.inputRouting.discardPointers()
        interactionProblem = nil
        terminal?.input.retire()
        let history = history
        let inspection = TerminalHistoryInspection(runtime: runtime, source: source,
            destination: destination,
            prepare: { await history?.waitUntilSettled() }) { [weak self] id, bookmark, failure in
                guard let self, self.historyInspection?.id == id else { return }
                self.historyInspection = nil
                if !self.stopped, self.presentationVisible {
                    self.historyReturnBookmark = bookmark
                    if let failure { self.interactionProblem = "The earlier viewing position could not be restored: \(failure)" }
                }
            }
        historyInspection = inspection
        return inspection
    }

    func returnFromHistorySearch() {
        guard !stopped, phase == .live, presentationVisible, !clipboardIsPresenting, !localInputNeedsDecision,
              historyReturn == nil, let bookmark = historyReturnBookmark, let runtime,
              bookmark.source == pump?.source else { return }
        historyReturnBookmark = nil; historyReturning = true
        terminal?.input.retire()
        let history = history
        historyReturn = Task { [weak self] in
            guard let self else { return }
            await history?.waitUntilSettled()
            do { try await runtime.restoreHistoryBookmark(bookmark, source: bookmark.source) }
            catch {
                if !self.stopped, self.presentationVisible {
                    self.interactionProblem = "The earlier viewing position could not be restored: \(Self.describe(error))"
                }
            }
            self.historyReturning = false; self.historyReturn = nil
        }
    }

    /// Closing a workspace or replacing its ended controller releases plaintext
    /// and prevents an in-flight teardown from capturing it again afterwards.
    func discardRetainedOutput() {
        retainEndedOutput = false; retainedOutput = nil; retainedOutputFailure = nil
    }

    /// The person has read what an ended connection left known about their input.
    func dismissInputDelivery() { inputDelivery = .init() }

    private func inputFailed(_ error: Error) {
        if let recovery = keyboard?.recovery,
           error is TerminalKeyboardError || (error as? TerminalInputError) == .capacity {
            localInputRecovery = recovery
            interactionProblem = nil
            return
        }
        // Rejected UIKit edits and local queue admission are not SSH failures.
        // In particular TerminalKeyboardError.capacity is a different type from
        // the coordinator's capacity error, and must not close a live terminal.
        if let failure = error as? TerminalKeyboardError {
            switch failure {
            case .capacity:
                interactionProblem = "The keyboard's local buffer is full. The rejected edit was not queued; earlier admitted input is unchanged."
            case .invalidRange, .encoding:
                interactionProblem = "This keyboard edit could not be encoded and was not queued. The connection is still open."
            case .blocked:
                interactionProblem = "The keyboard could not complete its local handoff. Pending input remains on its original terminal."
            case .unavailable, .transitionInProgress:
                interactionProblem = "The keyboard is unavailable or changing focus. This edit was not accepted."
            }
            return
        }
        // Rejected local text is not a transport failure. In particular an
        // oversized/unsafe clipboard must not tear down a working SSH session.
        switch error as? TerminalInputError {
        case .capacity:
            interactionProblem = "Input was not accepted because a local text or buffer limit was reached. No rejected input will be sent automatically."
        case .unsafePaste:
            interactionProblem = "The clipboard contains terminal control characters and was not pasted."
        case .emptyInput:
            interactionProblem = "There is no text to paste."
        default: fail(Self.describe(error))
        }
    }

    /// Observe the actual local queue without replacing the native responder's
    /// observer. One exact session owns this subscription through replacement.
    private func observeKeyboardRecovery(_ keyboard: TerminalKeyboardSession) throws {
        if let old = keyboardRecoveryObservation { old.keyboard.removeStateObserver(old.token) }
        keyboardRecoveryObservation = nil
        let token = try keyboard.observeState { [weak self, weak keyboard] in
            guard let self, let keyboard, !self.stopped, self.keyboard === keyboard else { return }
            if self.localInputRecovery != nil, keyboard.recovery == nil,
               keyboard.phase == .suspending || keyboard.phase == .retired,
               !self.resolvingLocalInput, self.presentationVisible {
                self.interactionProblem = "The keyboard was interrupted. Pending local input was discarded; earlier admitted input was not replayed."
            }
            self.localInputRecovery = keyboard.recovery
        }
        keyboardRecoveryObservation = (keyboard, token)
        localInputRecovery = keyboard.recovery
    }

    private func retireKeyboardRecovery() {
        localInputReset?.cancel()
        if let old = keyboardRecoveryObservation { old.keyboard.removeStateObserver(old.token) }
        keyboardRecoveryObservation = nil; localInputRecovery = nil
    }

    /// The buttons retain both the responder identity and its single-use blocked
    /// decision. A replacement pane/view or an old banner cannot consume them.
    private func recoveryKeyboard(_ recovery: TerminalKeyboardRecovery, retrying: Bool) throws
        -> (TerminalKeyboardSession, NativeTerminalView) {
        guard !stopped, phase == .live, presentationVisible, !overviewBlocking,
              !clipboardIsPresenting, !resolvingLocalInput, localInputReset == nil,
              paneHandoff == nil, viewAttachment == nil, requestedMuxPane == nil,
              focus?.shows(machineID: target.machineID) == true,
              let keyboard, let view = terminal, view.input.session === keyboard,
              keyboard.id == recovery.keyboardID, keyboard.originalFocus == recovery.focus,
              keyboard.recovery == recovery, pump?.source == recovery.focus.source,
              !retrying || view.renderer.admittedFrame?.isLive == true,
              connection?.muxControl.map({ $0.inputRouting.current.ready
                  && $0.inputRouting.current.destination == displayedMuxDestination }) ?? true else {
            throw TerminalInputError.stale
        }
        return (keyboard, view)
    }

    func retryLocalInput(_ recovery: TerminalKeyboardRecovery) {
        do {
            let (keyboard, view) = try recoveryKeyboard(recovery, retrying: true)
            try keyboard.retryLocalAdmission(expectedRecovery: recovery.id)
            interactionProblem = nil
            view.requestKeyboard(true)
        } catch {
            if !stopped { interactionProblem = "Pending input was not retried: \(Self.describe(error))" }
        }
    }

    func discardLocalInput(_ recovery: TerminalKeyboardRecovery) {
        do {
            let (keyboard, view) = try recoveryKeyboard(recovery, retrying: false)
            resolvingLocalInput = true
            do { try keyboard.discardLocalAdmission(expectedRecovery: recovery.id) }
            catch { resolvingLocalInput = false; throw error }
            // This retires the native responder, not the SSH connection. The
            // keyboard's existing shutdown owns releases for admitted held keys.
            view.input.retire()
            interactionProblem = nil
            let source = recovery.focus.source, mux = displayedMuxDestination
            localInputReset = Task { [weak self, weak view, keyboard] in
                await keyboard.waitUntilSuspended()
                guard let self else { return }
                defer { self.localInputReset = nil; self.resolvingLocalInput = false }
                guard !Task.isCancelled, !self.stopped, self.phase == .live,
                      self.presentationVisible, !self.overviewBlocking, !self.clipboardIsPresenting,
                      self.keyboard === keyboard, let view, self.terminal === view,
                      self.pump?.source == source, self.displayedMuxDestination == mux,
                      self.connection?.muxControl.map({ $0.inputRouting.current.ready
                          && $0.inputRouting.current.destination == mux }) ?? true,
                      self.focus?.shows(machineID: self.target.machineID) == true else { return }
                // A new keyboard is created only for this still-visible source.
                // No text, terminal opening, connection or route is retried here.
                await self.focus?.activateKeyboard()
            }
        } catch {
            if !stopped { interactionProblem = "Pending input was not discarded: \(Self.describe(error))" }
        }
    }

    /// Settle the old responder's composition and releases locally before asking
    /// for another pane. Admitted bytes keep their old writer; this does not wait
    /// for network delivery. The new pane gets a fresh view and focus only after
    /// its output has reached the canonical model.
    func showPane(_ paneID: Int) {
        guard let control = connection?.muxControl else { return }
        do { _ = try startPaneHandoff(control.inputRouting.navigation(pane: paneID)) }
        catch { if !stopped { interactionProblem = "The pane switch was not completed: \(Self.describe(error))" } }
    }

    /// A gesture's task owns and joins local keyboard cleanup. Its captured
    /// target is checked again after every potentially suspended preparation.
    func navigatePane(_ plan: TerminalMuxInputRouting.NavigationPlan) async throws {
        try Task.checkCancellation()
        guard let task = try startPaneHandoff(plan) else { return }
        await withTaskCancellationHandler { await task.value } onCancel: { task.cancel() }
        try Task.checkCancellation()
    }

    var paneNavigationBusy: Bool {
        paneHandoff != nil || viewAttachment != nil || requestedMuxPane != nil || localInputReset != nil || localInputNeedsDecision
    }

    private func startPaneHandoff(_ plan: TerminalMuxInputRouting.NavigationPlan) throws -> Task<Void, Never>? {
        let paneID = plan.paneID
        // A deliberate pane switch must not silently discard a blocked queue.
        // The operator resolves its original-source Retry/Discard decision first.
        guard !localInputNeedsDecision else { throw TerminalKeyboardError.blocked }
        guard !stopped, phase == .live, paneHandoff == nil, viewAttachment == nil,
              presentationVisible, !clipboardIsPresenting,
              focus?.shows(machineID: target.machineID) == true,
              let control = connection?.muxControl else { throw TerminalInputError.unavailable }
        try control.inputRouting.validateNavigation(plan)
        guard paneID != control.current.shown || requestedMuxPane != nil else { return nil }
        requestedMuxPane = paneID
        requestedMuxNavigation = plan
        paneHandoff = Task { [weak self] in
            guard let self else { return }
            defer {
                self.paneHandoff = nil
                // Selecting the already-shown pane can cancel an unprocessed
                // request and produce no new transport notification.
                self.muxInputChanged(control.inputRouting.current, control: control)
            }
            do {
                if let keyboard = self.keyboard, keyboard.acceptsInput {
                    let vacant = try await keyboard.handoff(to: nil)
                    // Ownership of the handoff result is ours even if hiding or
                    // cancellation raced its return. No invisible stream leaks.
                    _ = try await self.connections.coordinator.suspend(vacant)
                } else {
                    self.keyboard?.suspend()
                    await self.keyboard?.waitUntilSuspended()
                }
                try Task.checkCancellation()
                guard !self.stopped, self.phase == .live, self.presentationVisible,
                      self.connection?.muxControl === control,
                      control.current.panes.contains(where: { $0.paneId == paneID }) else {
                    throw TerminalInputError.stale
                }
                try control.inputRouting.validateNavigation(plan)
                // Retire, rather than merely pause, the old drawable/caret.
                // Its in-flight GPU completions cannot target the new view.
                self.presenter?.acceptsFrames = false
                self.terminal?.retire(); self.terminal = nil
                self.presenter?.view = nil
                self.displayedMuxDestination = nil; self.lastViewportRevision = 0
                let clipboard = self.clipboard
                clipboard?.retire(); self.clipboard = nil; self.presenter?.clipboard = nil
                await clipboard?.waitUntilSettled()
                if let presentation = self.presentation, !presentation.isRetired {
                    try presentation.request(false, source: presentation.source)
                    await presentation.waitUntilSettled()
                }
                if let history = self.history {
                    try history.request(top: nil, source: history.source)
                    await history.waitUntilSettled()
                }
                self.historyReturnBookmark = nil
                try Task.checkCancellation()
                guard !self.stopped, self.phase == .live, self.presentationVisible,
                      self.connection?.muxControl === control,
                      control.current.panes.contains(where: { $0.paneId == paneID }) else {
                    throw TerminalInputError.stale
                }
                try control.request(plan)
            } catch {
                self.requestedMuxPane = nil; self.requestedMuxNavigation = nil
                // The plan can expire after handoff succeeds but before the
                // old view is retired. Its responder is already gone; keeping
                // that drawable would block showView from rebuilding input.
                if self.keyboard?.phase == .retired {
                    self.presenter?.acceptsFrames = false
                    self.terminal?.retire(); self.terminal = nil; self.presenter?.view = nil
                    self.displayedMuxDestination = nil; self.lastViewportRevision = 0
                    let clipboard = self.clipboard
                    clipboard?.retire(); self.clipboard = nil; self.presenter?.clipboard = nil
                    await clipboard?.waitUntilSettled()
                }
                if !self.stopped, !(error is CancellationError) {
                    self.interactionProblem = "The pane switch was not completed: \(Self.describe(error))"
                }
                // A failed pre-barrier admission preserves the old responder.
                // A failure after retirement may rebuild only the still-current
                // ready pane, never the rejected destination.
            }
        }
        return paneHandoff
    }

    /// This callback is independent of pane-title/search changes and rejects
    /// reordered cross-executor notifications. Readiness is model-consumption
    /// evidence; NativeTerminalView still requires its new rendered caret.
    private func muxInputChanged(_ state: TerminalMuxInputRouting.State, control: TerminalMuxControl) {
        guard !stopped, connection?.muxControl === control,
              state.revision == control.inputRouting.current.revision else { return }
        if !state.ready || displayedMuxDestination != state.destination {
            terminal?.input.retire()
            presenter?.acceptsFrames = false
            terminal?.renderer.setSceneActive(false)
        }
        guard state.ready, !state.retired, let selected = state.destination else {
            if state.destination == nil, phase == .live {
                requestedMuxPane = nil; requestedMuxNavigation = nil
                interactionProblem = "The selected pane is no longer available. Choose another pane; pending input was not redirected."
            }
            return
        }
        if let requestedMuxPane, requestedMuxPane != selected.paneID { return }
        requestedMuxPane = nil; requestedMuxNavigation = nil
        guard paneHandoff == nil, focus?.shows(machineID: target.machineID) == true else { return }
        showView()
    }

    /// Metadata and readiness notifications are latest-value presentation, not
    /// a transcript or input authority. The observer reads them on MainActor,
    /// coalesces floods, and pauses UI work for offscreen machines. The exact
    /// control and routing revision are still checked before a responder changes.
    private func observeMux(_ control: TerminalMuxControl) {
        muxObservation = TerminalMuxObservation(control: control, initiallyActive: presentationVisible) {
            [weak self, weak control] state, routing in
            guard let self, let control, !self.stopped, self.connection?.muxControl === control else { return }
            self.muxState = state
            self.terminal?.remoteMouseGrabbed = state.mouseGrabbed
            if let notice = state.notice, self.noticedMux !== control {
                self.noticedMux = control
                self.interactionProblem = notice
            }
            if let rejected = state.rejectedNavigation, rejected == self.requestedMuxNavigation {
                self.requestedMuxPane = nil; self.requestedMuxNavigation = nil
                if self.presentationVisible {
                    self.interactionProblem = "The pane list or selection changed. The swipe was cancelled; choose the pane again."
                }
            }
            self.muxInputChanged(routing, control: control)
        }
        // The observation above sleeps while the machine is off screen; this
        // wakes only when the machine gains its first unread pane or reads
        // its last, and reads the current answer rather than a stale one.
        control.observeUnread { [weak self, weak control] _ in
            Task { @MainActor [weak self, weak control] in
                guard let self, let control, !self.stopped, self.connection?.muxControl === control else { return }
                self.unreadMuxPanes = !control.current.unread.isEmpty
            }
        }
    }

    /// The machine's mux, which searches its panes' scrollback on the server;
    /// nil for a session that is a shell of its own.
    var muxControl: TerminalMuxControl? { stopped ? nil : connection?.muxControl }

    /// The device's network took a new route. A live session probes its
    /// server now and is reported lost if one liveness interval passes
    /// unanswered, instead of the usual several.
    func networkRouteChanged() {
        guard !stopped, phase == .live, let connection else { return }
        Task { await connection.networkRouteChanged() }
    }

    /// A prepared transport is disposable only before any terminal-opening
    /// intent or resource exists. MainActor callers reserve the exact owner and
    /// seal it in the same turn; a stale Ready snapshot is not permission.
    var canReleaseUnusedPreparation: Bool {
        !stopped && !settled && phase == .ready && startup == nil && connection != nil
            && !terminalRequested && !explicitTerminalRequested && pump == nil && runtime == nil
            && unadoptedModel == nil && !attached && keyboard == nil && terminal == nil
            && viewAttachment == nil && paneHandoff == nil && run == nil && cleanup == nil
    }

    @discardableResult
    func requestReleaseUnusedPreparation() -> Bool {
        guard canReleaseUnusedPreparation else { return false }
        requestStop()
        return true
    }

    func setPresentationVisible(_ visible: Bool) {
        let visible = visible && !stopped
        presentationVisible = visible
        // A pending password belongs to this visible destination. Returning to
        // the same tab later must not revive its old prompt or queued attempt.
        if !visible, target.requiresAuthenticationPrompt, phase == .connecting { requestStop() }
        if visible { unseenShellOutput = false }
        muxObservation?.setActive(visible)
        // Only the machine on screen sizes its mux pane to the phone; the
        // others give their panes back to the desktop's layout.
        connection?.muxControl?.setVisible(visible)
        if !visible {
            pointerFocusActivation?.cancel()
            // A delayed view/focus build must not navigate back to a machine
            // the user has already left. Its owner remains retained to join.
            viewAttachment?.cancel(); paneHandoff?.cancel()
            localInputReset?.cancel()
            interactionProblem = nil
            historyInspection?.retire()
            historyReturn?.cancel(); historyReturnBookmark = nil
            connection?.muxControl?.clearSearch()
        }
        // Frames need a view. A session that connected in the background turns
        // visible when its view attaches (attachView calls this again), so
        // that transition draws its first frame instead of one nobody saw.
        let muxReady = connection?.muxControl.map {
            $0.inputRouting.current.ready && $0.inputRouting.current.destination == displayedMuxDestination
        } ?? true
        let shown = visible && terminal != nil && requestedMuxPane == nil && muxReady
        clipboard?.setActive(shown)
        presenter?.acceptsFrames = shown
        terminal?.renderer.setSceneActive(shown)
        if let presentation, !presentation.isRetired {
            do { try presentation.request(shown, source: presentation.source) }
            catch { fail(Self.describe(error)) }
        }
    }

    /// Installs only the source-matching keyboard published by navigation.
    func install(_ keyboard: TerminalKeyboardSession, showKeyboard: Bool = true) {
        guard !stopped, phase == .live, let terminal,
              terminal.input.session !== keyboard else { return }
        guard keyboard.originalFocus.source == pump?.source else {
            fail("The selected keyboard belongs to another terminal."); return
        }
        // An activation may have started before a read-only sheet opened and
        // completed its handoff afterwards. Seal that newly published responder
        // before it can acquire hardware input behind the inspection surface.
        guard !overviewBlocking, !keyboardActivationBlocked, paneHandoff == nil, requestedMuxPane == nil,
              connection?.muxControl.map({ $0.inputRouting.current.ready
                  && $0.inputRouting.current.destination == displayedMuxDestination }) ?? true else {
            keyboard.suspend()
            focus?.inputRetired(machineID: target.machineID)
            return
        }
        do {
            try terminal.replaceInputSession(keyboard)
            self.keyboard = keyboard
            try observeKeyboardRecovery(keyboard)
            clipboard?.inputInstalled()
            terminal.requestKeyboard(showKeyboard)
        } catch { keyboard.suspend(); fail(Self.describe(error)) }
    }

    /// Authentication and terminal creation are separate admissions. A group
    /// can prepare an authenticated connection without creating a remote shell
    /// or running the mux command. Selection is checked again after prompts.
    func start(openTerminal: Bool = true) async {
        terminalRequested = terminalRequested || openTerminal
        guard phase == .idle, !stopped else { return }
        await startOwnedOperation()
    }

    /// A committed selection requests on-select/existing-only activation; the
    /// explicit Open Terminal action also authorizes a manual route. Requests
    /// arriving while authentication is suspended share its existing owner.
    func activateTerminal(explicit: Bool = false) async {
        guard !stopped else { return }
        terminalRequested = true
        explicitTerminalRequested = explicitTerminalRequested || explicit
        if phase == .idle { await start(); return }
        if let startup { await startup.value }
        guard !stopped, phase == .ready, shouldOpenTerminal else { return }
        await startOwnedOperation()
    }

    /// The app actually entered the background while remote creation was in
    /// flight. Join the same operation through normal cleanup; a potentially
    /// sent request keeps its journal evidence and is never silently retried.
    /// Authentication prompts and already-live sessions have separate owners.
    func suspendPendingTerminalCreation() {
        guard phase == .connecting, connection != nil, terminalRequested else { return }
        startup?.cancel()
    }

    private var shouldOpenTerminal: Bool {
        guard focus?.permitsTerminalCreation == true, terminalRequested,
              route.sessionCreation.permitsOpening(explicit: explicitTerminalRequested) else { return false }
        if explicitTerminalRequested { return true }
        // Attaching to existing mux panes, on a mux route or a direct route's
        // default mux, starts no shell. A shell after one ended or was lost,
        // including the fallback when a reattached mux is gone, waits for
        // New Session.
        guard route.mux || connection?.muxControl != nil
                || focus?.requiresNewSession(target.machineID) == false else { return false }
        return focus?.shows(machineID: target.machineID) == true
    }

    /// Each operation retains all partially constructed resources. Teardown
    /// cancels and joins it; a late connection/view cannot outlive a settled UI.
    private func startOwnedOperation() async {
        phase = .connecting
        let id = UUID()
        startupID = id
        let job = Task { await self.performStart() }
        startup = job
        await withTaskCancellationHandler { await job.value } onCancel: { job.cancel() }
        // A Ready activation can resume first and reserve a newer operation
        // while this completed authentication wrapper is still suspended.
        // Its connecting phase belongs to that new owner, not failed startup.
        guard startupID == id else { return }
        startup = nil; startupID = nil
        if Task.isCancelled { requestStop() }
        if stopped || (phase != .live && phase != .ready) { await teardown() }
    }

    private func checkStarting() throws {
        try Task.checkCancellation()
        guard !stopped else { throw CancellationError() }
    }

    /// Never calls teardown: teardown must be able to join this task without a
    /// self-join. Retain each resource BEFORE the next cancellation check/await.
    private func performStart() async {
        do {
            try checkStarting()
            let prepared: WorkspaceSSHConnection
            // Preparing acknowledges an ended session too (disconnected, or cut
            // off when iOS ended the app), or Connect could never reach it.
            if let connection { prepared = connection }
            else { prepared = try await connectAcknowledgingEarlierSessions() }
            var attempt = prepared
            self.connection = attempt
            try checkStarting()
            guard shouldOpenTerminal else { phase = .ready; return }
            let geometry = try TerminalSessionGeometry(columns: 80, rows: 24)
            let pump: TerminalSessionPump
            do { pump = try await attempt.openTerminal(geometry: geometry) }
            catch let failure as TerminalMuxError {
                // No mux answered, it speaks a protocol this app does not (a
                // newer desktop's), or it has no panes: the machine gets a
                // shell of its own on one of its direct SSH routes instead, as
                // before there was a mux. That shell opens only under the
                // direct route's own creation policy (shouldOpenTerminal); an
                // existing-only mux route grants nothing, and a machine with
                // no direct route keeps the mux's answer.
                guard target.routes.contains(where: { !$0.mux }) else { throw failure }
                Self.log.info("no usable mux on \(self.target.machineID, privacy: .public) (\(String(describing: failure), privacy: .public)); opening a shell")
                // A failed cleanup retains this exact owner; it may not be
                // hidden by opening another key, route, or shell.
                try await connections.manager.close(connectionID: attempt.id)
                self.connection = nil
                if rememberPasswordMuxFallback() {
                    throw ConnectFailure(help: .init(title: "Couldn't attach to \(target.machineName)",
                        message: "This machine's FrankenTerm mux is unavailable. Choose Retry to open its direct SSH route; you will sign in again. No new shell has been opened.",
                        remedy: .none))
                }
                shellOnly = true
                try checkStarting()
                attempt = try await connectAcknowledgingEarlierSessions()
                self.connection = attempt
                try checkStarting()
                guard shouldOpenTerminal else { phase = .ready; return }
                pump = try await attempt.openTerminal(geometry: geometry)
            }
            let connection = attempt
            self.pump = pump
            // An earlier launch ended, killed by iOS or the person, before
            // the machine answered input sent to it.
            if unansweredInput.takeFromEarlierLaunch(), !inputDelivery.unanswered { inputDelivery.endedUnanswered() }
            if let control = connection.muxControl {
                attachedToMux = true
                // The last connection to this pane, perhaps before a relaunch,
                // ended before its server confirmed input sent to it.
                if control.current.inputUnconfirmed, !inputDelivery.unanswered { inputDelivery.endedUnanswered() }
                observeMux(control)
                control.setVisible(presentationVisible)
            }
            try checkStarting()
            let presenter = TerminalFramePresenter(source: pump.source, connection: connection)
            presenter.onUnavailable = { [weak self] reason in
                self?.fail("The display is unavailable (\(Self.describe(reason))).")
            }
            presenter.onInterruptedInput = { [weak self] outcomes in
                guard let self else { return }
                var delivery = self.inputDelivery
                delivery.record(outcomes, serverConfirms: self.attachedToMux)
                if delivery != self.inputDelivery { self.inputDelivery = delivery }
            }
            self.presenter = presenter
            let model = try NativeTerminalModel(source: pump.source, geometry: geometry)
            unadoptedModel = model
            if let palette = basePalette { try model.setBasePalette(palette); appliedPalette = palette }
            let coordinator = connections.coordinator
            // A mux pane's output is the mux's to report: output still on its
            // way when the machine leaves the screen was already seen.
            let hiddenOutput: (@Sendable () -> Void)? = connection.muxControl != nil ? nil : { @Sendable [weak self] in
                Task { @MainActor [weak self] in
                    guard let self, !self.stopped, !self.presentationVisible else { return }
                    self.unseenShellOutput = true
                }
            }
            let unansweredInput = unansweredInput
            let runtime = TerminalModelRuntime(model: model, coordinator: coordinator,
                                               delivery: presenter, initiallyVisible: false,
                                               hiddenOutput: hiddenOutput,
                                               awaitingAnswer: { unansweredInput.set($0) })
            self.runtime = runtime; unadoptedModel = nil
            let source = pump.source
            let resize = TerminalViewportResize(source: source, initial: geometry) { geometry in
                do {
                    try await connection.requestResize(geometry, source: source)
                    return .queued
                } catch NativeSSHError.busy { return .busy }
            }
            self.resize = resize
            resize.onFailure = { [weak self] error in self?.fail(Self.describe(error)) }
            // The model keeps the selected history row while output arrives and
            // while this machine is hidden, so returning finds the same place.
            let history = TerminalHistorySelection(source: source) { top, source in
                try await runtime.setViewport(top: top, source: source)
            }
            self.history = history
            history.onFailure = { [weak self] error in self?.fail(Self.describe(error)) }
            let presentation = TerminalPresentationVisibility(source: source, initiallyVisible: false) { visible, source in
                try await runtime.setVisible(visible, source: source)
            }
            self.presentation = presentation
            presentation.onFailure = { [weak self] error in self?.fail(Self.describe(error)) }
            phase = .live
            dropped = false
            let run = Task {
                try await pump.run(consumer: runtime) { geometry, appliedSource in
                    // Retirement can race the MainActor hop after the pump's
                    // stop check. Ignore only this retired owner, never a source
                    // mismatch or an invalid model-completion notification.
                    try await MainActor.run {
                        guard !resize.isRetired else { return }
                        try resize.didApply(geometry, source: appliedSource)
                    }
                }
            }
            self.run = run
            // The observer is NOT the task joined by teardown. A pump ending
            // naturally can therefore request cleanup without awaiting itself.
            runObserver = Task { [weak self] in
                let result = await run.result
                await self?.pumpEnded(result)
            }
            // The shell runs and its output arrives from here on. The machine on
            // screen gets its view now; one connecting in the background gets
            // it when the user switches to it (showView).
            if focus?.shows(machineID: target.machineID) == true {
                showView()
                await viewAttachment?.value
            }
        } catch TerminalSessionJournalError.reviewRequired {
            if !stopped { phase = .failed("The previous session on this machine is still closing. Try again in a moment.") }
        } catch let failure as ConnectFailure {
            if !stopped { dropped = retriesDrop && !failure.answered; phase = .couldNotConnect(failure.help) }
        } catch {
            if !stopped { phase = .failed(Self.describe(error)) }
        }
    }

    /// Puts a live session on screen: the scene's keyboard, the Metal view and
    /// what follows the view. Idempotent; teardown joins it.
    func showView() {
        guard !stopped, !overviewBlocking, phase == .live, presentationVisible, !clipboardIsPresenting,
              focus?.shows(machineID: target.machineID) == true,
              terminal == nil, viewAttachment == nil,
              paneHandoff == nil, requestedMuxPane == nil,
              connection?.muxControl?.inputRouting.current.ready != false else { return }
        viewAttachment = Task { [weak self] in
            await self?.attachView()
            self?.viewAttachment = nil
            // A -> B -> A may return before A's cancelled build joins. Retry
            // only after that exact owner ends, and only if A is current again.
            self?.showView()
        }
    }

    private func attachView() async {
        guard let pump, let runtime, let presenter else { return }
        let expectedMux = connection?.muxControl?.inputRouting.current.destination
        func checkLive() throws {
            try Task.checkCancellation()
            guard !stopped, !overviewBlocking, phase == .live, presentationVisible, !clipboardIsPresenting,
                  focus?.shows(machineID: target.machineID) == true,
                  requestedMuxPane == nil,
                  connection?.muxControl?.inputRouting.current.destination == expectedMux,
                  connection?.muxControl?.inputRouting.current.ready != false else { throw CancellationError() }
        }
        do {
            try checkLive()
            guard let focus else { throw TerminalKeyboardError.unavailable }
            attached = true
            let keyboard = try await focus.attach(machineID: target.machineID, source: pump.source, encoder: runtime)
            self.keyboard = keyboard
            try observeKeyboardRecovery(keyboard)
            try checkLive()
            // Recreated pane views continue the canonical font revision rather
            // than resetting it to one after a prior pinch/text-size change.
            await fontRevisionChange?.value
            try checkLive()
            var configuration = Self.renderConfiguration
            configuration.fontRevision = appliedFontRevision
            let view = try await NativeTerminalView(session: keyboard, configuration: configuration)
            terminal = view
            try checkLive()
            displayedMuxDestination = expectedMux
            presenter.view = view
            let clipboard = try NativeTerminalClipboardController(terminal: view,
                destinationLabel: "\(target.machineName)\nMachine \(target.machineID) · Route \(route.id)"
                    + (expectedMux.map { " · Pane \($0.paneID)" } ?? ""),
                commandDestination: TerminalCommandDraftDestination(source: pump.source, mux: expectedMux))
            self.clipboard = clipboard; presenter.clipboard = clipboard
            clipboard.onFailure = { [weak self] error in self?.interactionProblem = Self.describe(error) }
            let source = pump.source
            clipboard.onCommandDraftFocus = { [weak self, weak view] in
                guard let self, let view else { throw TerminalCommandDraftError.unavailable }
                return try await self.restoreCommandDraftFocus(view: view, source: source, destination: expectedMux)
            }
            view.onHistoryViewportRequest = { [weak self] top in self?.selectHistory(top) }
            view.onKeyboardRequest = { [weak self] in
                guard let focus = self?.focus else { return }
                Task { await focus.activateKeyboard() }
            }
            view.onFontSizeChange = { [weak self] size in self?.focus?.fontSizeChanged(size) }
            view.onFontRevision = { [weak self] revision in self?.applyFontRevision(revision) }
            view.onViewportChange = { [weak self] viewport in self?.propose(viewport) }
            if let control = connection?.muxControl, let expectedMux {
                // The immutable view/visit pair survives a delayed callback;
                // it must never be replaced with the newly selected pane.
                view.onRemoteScroll = { [weak self, weak control, weak view] steps, column, row in
                    guard let self, let control, let view else { return }
                    self.submitPointer(.wheel(steps: steps, column: column, row: row),
                        destination: expectedMux, control: control, view: view)
                }
                view.remoteMouseGrabbed = control.current.mouseGrabbed
                view.onRemoteClick = { [weak self, weak control, weak view] column, row in
                    guard let self, let control, let view else { return }
                    self.submitPointer(.click(column: column, row: row),
                        destination: expectedMux, control: control, view: view)
                }
            } else {
                view.onRemoteScroll = { [weak self, weak view] steps, column, row in
                    guard let self, let view else { return }
                    self.submitDirectPointer(.wheel(steps: steps), column: column, row: row, view: view)
                }
                view.onRemoteClick = { [weak self, weak view] column, row in
                    guard let self, let view else { return }
                    self.submitDirectPointer(.click, column: column, row: row, view: view)
                }
            }
            view.onFailure = { [weak self, weak view] error in
                guard let self, let view, self.terminal === view else { return }
                self.inputFailed(error)
            }
            view.onInputRetired = { [weak self] in
                guard let self else { return }
                self.connection?.muxControl?.inputRouting.discardPointers()
                self.clipboard?.inputRetired()
                self.focus?.inputRetired(machineID: self.target.machineID)
            }
            view.onKeyboardDismissed = { [weak self, weak view] in
                guard let self, let view else { return }
                self.restorePointerFocus(afterHiding: view)
            }
            setPresentationVisible(presentationVisible)
            view.requestKeyboard(presentationVisible)
        } catch {
            let superseded = Task.isCancelled || stopped || overviewBlocking || !presentationVisible || clipboardIsPresenting
                || focus?.shows(machineID: target.machineID) != true
                || connection?.muxControl?.inputRouting.current.destination != expectedMux
            if superseded || error is CancellationError {
                terminal?.retire(); terminal = nil; presenter.view = nil
                keyboard?.suspend(); focus?.inputRetired(machineID: target.machineID)
            } else { fail(Self.describe(error)) }
        }
    }

    private func submitPointer(_ action: TerminalMuxInputRouting.PointerAction,
                               destination: TerminalMuxInputRouting.Destination,
                               control: TerminalMuxControl, view: NativeTerminalView) {
        guard !stopped, !overviewBlocking, phase == .live, presentationVisible, !clipboardIsPresenting,
              paneHandoff == nil, terminal === view, connection?.muxControl === control,
              displayedMuxDestination == destination else { return }
        do {
            switch action {
            case .click(let column, let row):
                try control.click(column: column, row: row, destination: destination)
            case .wheel(let steps, let column, let row):
                try control.scroll(steps: steps, column: column, row: row, destination: destination)
            }
        } catch TerminalInputError.stale {
            // A retired or not-yet-presentable visit is not a broken session.
        } catch {
            interactionProblem = "The gesture was not accepted: \(Self.describe(error)). It will not be replayed."
        }
    }

    private func submitDirectPointer(_ action: TerminalPointerAction, column: Int, row: Int,
                                     view: NativeTerminalView) {
        guard !stopped, !overviewBlocking, phase == .live, presentationVisible,
              !clipboardIsPresenting, !localInputNeedsDecision, terminal === view,
              view.isSceneActive, connection?.muxControl == nil,
              focus?.shows(machineID: target.machineID) == true,
              let projection = view.renderer.pointerProjection else { return }
        do {
            let event = try TerminalPointerEvent(action: action, column: column, row: row, frame: projection.frame)
            try view.input.session.pointer(event)
        } catch TerminalInputError.stale {
            // The original grid has gone away; no mouse bytes were admitted.
        } catch {
            interactionProblem = "The gesture was not accepted: \(Self.describe(error)). Try it again when terminal input is ready."
        }
    }

    /// Dismissing the software keyboard retires its IME callbacks and held
    /// keys first. One owned activation then publishes a new same-source focus
    /// for pointer input without making its responder first responder.
    private func restorePointerFocus(afterHiding view: NativeTerminalView) {
        guard pointerFocusActivation == nil, !stopped, phase == .live,
              terminal === view, presentationVisible, view.isSceneActive,
              !overviewBlocking, !keyboardActivationBlocked,
              focus?.shows(machineID: target.machineID) == true else { return }
        let source = view.source, activity = view.sceneActivityGeneration
        pointerFocusActivation = Task { [weak self, weak view] in
            guard let self else { return }
            defer { self.pointerFocusActivation = nil }
            guard !Task.isCancelled, let view, self.terminal === view, view.isSceneActive,
                  view.sceneActivityGeneration == activity,
                  self.presentationVisible, !self.stopped, !self.overviewBlocking,
                  !self.keyboardActivationBlocked else { return }
            await self.focus?.activateKeyboard(showKeyboard: false, source: source)
        }
    }

    /// Earlier sessions on this machine are acknowledged as the new one
    /// starts: their records only note that input may have been in flight
    /// when they ended, and asking about it first just got in the way. A
    /// record whose own cleanup is still running holds the start briefly.
    private func connectAcknowledgingEarlierSessions() async throws -> WorkspaceSSHConnection {
        for attempt in 0..<20 {
            for record in try await connections.journal.records()
                where record.requiresReview
                    && record.scope.workspaceID == target.workspaceID && record.scope.machineID == target.machineID {
                try checkStarting()
                guard record.canResolve else {
                    Self.log.info("earlier session on \(self.target.machineID, privacy: .public) (\(record.state.rawValue, privacy: .public)) is still open")
                    continue
                }
                do { try await connections.journal.resolve(record) }
                catch {
                    Self.log.error("earlier session on \(self.target.machineID, privacy: .public) not acknowledged: \(String(describing: error), privacy: .public)")
                }
            }
            do { return try await connectFirstReachable() }
            catch TerminalSessionJournalError.reviewRequired where attempt < 19 {
                try await Task.sleep(for: .milliseconds(250))
            }
        }
        throw TerminalSessionJournalError.reviewRequired
    }

    /// `answered`: the machine or its jump host gave an answer a retry would
    /// get again (another host key, every key refused), or this phone's own
    /// policy refused. That never retries. Any other failure (no route
    /// answered, a reset or timeout before sign-in finished) ends a retry as
    /// another drop, and each retry checks the host key before signing in.
    private struct ConnectFailure: Error { let help: WorkspaceConnectionHelp; var answered = true }

    private static func isAnswer(_ error: Error) -> Bool {
        switch error {
        case let failure as SSHAuthenticationError: failure != .busy
        case let failure as NativeSSHError: failure == .invalidInput || failure.isMachineAnswer
        case let failure as WorkspaceSSHConnectionError:
            failure == .capacity || failure == .stale || failure == .promptedHopRetryRequired
        default: true
        }
    }
    /// Each failed route, for `idevicesyslog` or Console: the screen shows
    /// only the explanation.
    private static let log = Logger(subsystem: "org.frankenterm.mobile", category: "connect")

    /// A new, fully authorized transport for each configured key. Only a proven
    /// unsigned public-key refusal advances within a route. Routes change only
    /// for initial reachability failures, before any key has been refused.
    private func connectFirstReachable() async throws -> WorkspaceSSHConnection {
        let onTailnet = WorkspaceRouteNetwork.deviceTailnetAddress() != nil
        let device = "this \(UIDevice.current.model)"
        var unreachable: [WorkspaceConnectionHelp] = []
        let candidates = target.attemptOrder().filter { !(shellOnly && $0.mux) }
        guard !candidates.isEmpty else {
            throw ConnectFailure(help: .init(title: "Couldn't connect to \(target.machineName)",
                message: "No direct SSH route is ready to open a new shell on this device.", remedy: .none))
        }
        var plan = try makeRouteAttemptPlan(candidates: candidates)
        while let attempt = plan.current {
            try checkStarting()
            guard let candidate = candidates.first(where: { $0.id == attempt.selection.routeID }) else {
                throw TerminalRouteAttemptPlanError.invalidRoutes
            }
            route = candidate
            credentialID = attempt.selection.credentialID
            do {
                let connected = try await connections.manager.connect(workspaceID: target.workspaceID,
                    expectedRevision: target.revision, machineID: target.machineID, routeID: candidate.id,
                    authentication: attempt.selection.authentication, confirmedByUser: true,
                    attachMux: !shellOnly && (!candidate.requiresAuthenticationPrompt || candidate.mux))
                // Retain the owner before any following operation can throw.
                // performStart checks cancellation before opening the channel.
                connection = connected
                authenticatedSelection = try plan.connected(attempt)
                passwordHopRetryPlan = nil; passwordMuxRetry = false
                return connected
            } catch is CancellationError {
                passwordHopRetryPlan = nil; passwordMuxRetry = false
                plan.cancel()
                throw CancellationError()
            } catch TerminalSessionJournalError.reviewRequired {
                try checkStarting(); plan.cancel()
                throw TerminalSessionJournalError.reviewRequired
            } catch {
                // The awaited manager owns and settles the failed transport.
                // Local cancellation wins before another candidate is selected.
                try checkStarting()
                Self.log.error("route \(candidate.id, privacy: .public) sign-in failed: \(String(describing: error), privacy: .public)")
                if error as? WorkspaceSSHConnectionError == .promptedHopRetryRequired {
                    let hasNext = try rememberPasswordHopRefusal(attempt, plan: &plan)
                    throw ConnectFailure(help: .init(title: "Couldn't connect to \(target.machineName)",
                        message: hasNext
                            ? "The destination refused this key. Choose Retry to try the next configured key; you will sign into the jump host again."
                            : "The destination refused all available configured keys. Add the intended public key to ~/.ssh/authorized_keys on the destination before retrying.",
                        remedy: .none))
                }
                if let failure = error as? NativeSSHError, failure.isCredentialRefused {
                    if try plan.failed(.credentialRefused, attempt: attempt) != nil { continue }
                    throw ConnectFailure(help: .init(title: "Couldn't connect to \(target.machineName)",
                        message: "The machine refused all available configured keys for this route. Add the intended key's public half to ~/.ssh/authorized_keys on the machine.", remedy: .none))
                }
                if let failure = error as? NativeSSHError, failure.isUnreachable {
                    let machine = candidate.jumpHost == nil ? target.machineName : "The jump host for \(target.machineName)"
                    let help = WorkspaceConnectionHelp(failure: failure, machine: machine, host: candidate.networkHost,
                        port: candidate.networkPort, onTailnet: onTailnet, device: device)
                        ?? .init(title: "Couldn't connect to \(target.machineName)", message: Self.describe(error), remedy: .none)
                    if plan.hasCredentialRefusal {
                        if candidate.promptingJump { passwordHopRetryPlan = plan }
                        _ = try plan.failed(.unreachable, attempt: attempt)
                        throw ConnectFailure(help: help)
                    }
                    unreachable.append(help)
                    if try plan.failed(.unreachable, attempt: attempt) != nil { continue }
                    break
                }
                _ = try plan.failed(.terminal, attempt: attempt)
                throw ConnectFailure(help: .init(title: "Couldn't connect to \(target.machineName)",
                                                 message: Self.describe(error), remedy: .none),
                                     answered: Self.isAnswer(error))
            }
        }
        throw ConnectFailure(help: WorkspaceConnectionHelp.best(unreachable)
            ?? .init(title: "Couldn't connect to \(target.machineName)", message: "No route answered.", remedy: .none),
            answered: false)
    }

    func makeRouteAttemptPlan(candidates: [TerminalSessionRoute]) throws -> TerminalRouteAttemptPlan {
        let candidates = candidates.filter { !(shellOnly && $0.mux) }
        if let pending = passwordHopRetryPlan, let selected = pending.current?.selection,
           candidates.contains(where: { $0.id == selected.routeID && $0.choices.contains(selected.authentication) }) {
            passwordHopRetryPlan = nil
            return pending
        }
        passwordHopRetryPlan = nil
        // Ordinary reconnects prefer the authenticated key. A fresh explicit
        // attempt otherwise starts in configured order within this target.
        let prior = authenticatedSelection ?? reconnectSelection
        let preferred = prior.flatMap { selected in
            candidates.contains(where: { $0.id == selected.routeID && $0.choices.contains(selected.authentication) })
                ? selected : nil
        }
        return try TerminalRouteAttemptPlan(routes: candidates.map {
            .init(routeID: $0.id, authenticationChoices: $0.choices)
        }, preferring: preferred)
    }

    /// Called only for the factory's positive unsigned destination-key refusal,
    /// never for a failed hop, password, signature or ambiguous network result.
    @discardableResult
    func rememberPasswordHopRefusal(_ attempt: TerminalRouteAttemptPlan.Attempt,
                                    plan: inout TerminalRouteAttemptPlan) throws -> Bool {
        guard target.routes.contains(where: { $0.id == attempt.selection.routeID && $0.promptingJump }),
              case .publicKey = attempt.selection.authentication else { throw TerminalRouteAttemptPlanError.staleAttempt }
        let next = try plan.failed(.credentialRefused, attempt: attempt)
        passwordHopRetryPlan = next == nil ? nil : plan
        return next != nil
    }

    /// Existing-only mux failure grants no new login or shell. Preserve only
    /// the direct-route choice for the person's next explicit Retry.
    @discardableResult
    func rememberPasswordMuxFallback() -> Bool {
        guard route.requiresAuthenticationPrompt, target.routes.contains(where: { !$0.mux }) else { return false }
        shellOnly = true; passwordMuxRetry = true
        return true
    }

    /// The desktop's default terminal font, bundled with its license, when it
    /// is registered; otherwise Kit's Menlo default, because Core Text would
    /// replace an unknown name with a proportional system font.
    static var renderConfiguration: NativeTerminalRenderConfiguration {
        var configuration = NativeTerminalRenderConfiguration()
        if UIFont(name: "PragmasevkaNF", size: 12) != nil { configuration.fontName = "PragmasevkaNF" }
        let saved = UserDefaults.standard.double(forKey: fontSizeKey)
        if NativeTerminalView.fontSizes.contains(saved) { configuration.fontPointSize = saved }
        return configuration
    }
    /// The text size the user last chose, in points, for every terminal.
    static let fontSizeKey = "terminal.fontPointSize"

    /// Shows this terminal's text at `size` points.
    func setFontSize(_ size: Double) { terminal?.setFontPointSize(size) }

    /// The renderer took a new font; the model's next frames must say so or
    /// they are not drawn. One change at a time reaches the runtime, and the
    /// newest revision wins (a pinch makes several).
    private func applyFontRevision(_ revision: UInt64) {
        wantedFontRevision = max(wantedFontRevision, revision)
        guard fontRevisionChange == nil else { return }
        fontRevisionChange = Task { [weak self] in
            while let self, !self.stopped, let runtime = self.runtime, let source = self.pump?.source,
                  self.wantedFontRevision > self.appliedFontRevision {
                let wanted = self.wantedFontRevision
                do {
                    try await runtime.setFontRevision(wanted, source: source)
                    self.appliedFontRevision = wanted
                } catch {
                    if !self.stopped { self.fail(Self.describe(error)) }
                    break
                }
            }
            self?.fontRevisionChange = nil
        }
    }

    /// User-facing text: an error's own description when it has one.
    static func describe(_ error: any Error) -> String {
        (error as? LocalizedError)?.errorDescription ?? String(describing: error)
    }

    /// Only a reported nonzero exit code or signal names how the shell ended;
    /// libssh2 cannot tell a reported zero from an absent exit status.
    static func describe(_ end: TerminalSessionEnd) -> String {
        switch end {
        case let .completed(completion):
            switch completion.remoteExit {
            case let .exitStatus(status): "The shell exited with status \(status)."
            case let .signal(signal): "The shell ended on signal \(signal)."
            case nil: "The shell exited."
            }
        case .peerOutputEnded: "The machine closed the session."
        case .cancelled: "Disconnected."
        case .disconnected: "The connection to the machine was lost."
        case .transportFailed: "The connection to the machine failed."
        case .consumerFailed: "The terminal couldn't show this session's output."
        case .ownershipConflict: "Another window took over this session."
        }
    }

    /// One latest-value owner coalesces layouts until the pump reports actual
    /// canonical application. There is no timer, optimistic model resize, or
    /// generation-less retry that can target a replacement terminal.
    private func propose(_ viewport: NativeTerminalViewport) {
        guard !stopped, phase == .live, let resize, viewport.source == resize.source,
              viewport.revision > lastViewportRevision,
              let rows = UInt32(exactly: viewport.rows),
              // At most TerminalSessionGeometry.maximumShownCells: the model
              // keeps both screens of it and the history that still fits.
              let columns = UInt32(exactly: viewport.columns),
              viewport.pixelWidth >= 0, viewport.pixelHeight >= 0,
              let geometry = try? TerminalSessionGeometry(columns: columns, rows: rows,
                  pixelWidth: UInt32(min(viewport.pixelWidth, 65_535)),
                  pixelHeight: UInt32(min(viewport.pixelHeight, 65_535))) else { return }
        do {
            try resize.propose(geometry, source: viewport.source)
            lastViewportRevision = viewport.revision
        } catch { fail(Self.describe(error)) }
    }

    private func selectHistory(_ top: Int64?) {
        guard !stopped, phase == .live, !clipboardIsPresenting, let history else { return }
        do { try history.request(top: top, source: history.source) }
        catch { fail(Self.describe(error)) }
    }

    private func sealLocalOwnership() {
        pointerFocusActivation?.cancel()
        presentationVisible = false
        retireKeyboardRecovery()
        muxObservation?.retire()
        connection?.muxControl?.inputRouting.discardPointers()
        muxSearchPresented = false
        clipboard?.retire()
        historyInspection?.retire()
        historyReturn?.cancel(); historyReturnBookmark = nil
        paletteChange?.cancel()
        fontRevisionChange?.cancel(); paneHandoff?.cancel()
        requestedMuxPane = nil; requestedMuxNavigation = nil
        connection?.muxControl?.inputRouting.removeObserver()
        interactionProblem = nil
        presenter?.acceptsFrames = false
        presentation?.retire()
        terminal?.retire()
        keyboard?.suspend()
        resize?.retire()
        history?.retire()
        pump?.requestStop()
    }

    /// Synchronous disappearance/close barrier, before SwiftUI schedules cleanup.
    func requestStop() {
        passwordHopRetryPlan = nil; passwordMuxRetry = false
        stopCurrentOperation()
    }

    private func stopCurrentOperation() {
        if !stopped, phase == .connecting || phase == .ready || phase == .live {
            phase = .ended(Self.describe(TerminalSessionEnd.cancelled))
        }
        stopped = true
        startup?.cancel(); viewAttachment?.cancel()
        sealLocalOwnership()
    }

    /// A display that cannot show this session's output must not keep sending
    /// it input. The pump's observer joins cleanup outside its own run task.
    private func fail(_ message: String) {
        guard !stopped, phase == .live else { return }
        phase = .failed(message)
        sealLocalOwnership()
    }

    private func pumpEnded(_ end: Result<TerminalSessionEnd, Error>) async {
        guard !stopped else { return }
        if phase == .live {
            switch end {
            case let .success(reason):
                switch reason {
                case .disconnected, .transportFailed: dropped = true
                default: break
                }
                // The mux server's own reason, when it ended the session.
                if let failure = connection?.muxControl?.current.failure {
                    phase = .ended("The machine's FrankenTerm mux ended the session: \(failure)")
                } else {
                    phase = .ended(Self.describe(reason))
                }
            case let .failure(error):
                dropped = error is TerminalTransportError || (error as? NativeSSHError)?.isConnectionFailure == true
                phase = .failed(Self.describe(error))
            }
        }
        await teardown()
    }

    func stop() async { requestStop(); await teardown() }
    func retryCleanup() async { guard !settled else { return }; await teardown() }

    /// All callers join one cleanup job. Failure retains the actual owners, and
    /// a later explicit retry resumes cleanup, not connection or shell creation.
    private func teardown() async {
        guard !settled else { return }
        stopCurrentOperation()
        let owned: (id: UUID, task: Task<Void, Never>)
        if let cleanup { owned = cleanup }
        else {
            let id = UUID()
            let task = Task { await self.performTeardown() }
            owned = (id, task); cleanup = owned
        }
        await owned.task.value
        if cleanup?.id == owned.id { cleanup = nil }
    }

    private func performTeardown() async {
        cleanupFailure = nil
        // Interrupt an already-published native connection promptly. Startup may
        // still be in openTerminal/view creation; it never joins this cleanup.
        if let connection { try? await connections.manager.close(connectionID: connection.id) }
        if let startup { await startup.value }
        startup = nil
        if let viewAttachment { await viewAttachment.value }
        if let localInputReset { await localInputReset.value; self.localInputReset = nil }
        if let pointerFocusActivation { await pointerFocusActivation.value; self.pointerFocusActivation = nil }
        if let paneHandoff { await paneHandoff.value; self.paneHandoff = nil }
        sealLocalOwnership()
        if let muxObservation { await muxObservation.close(); self.muxObservation = nil }
        var failure: Error?
        if let connection {
            // Native I/O stops now; a bound epoch deliberately refuses final
            // publication until responders and outcome delivery have joined.
            try? await connections.manager.close(connectionID: connection.id)
        }
        if let pump {
            do { try await pump.disconnect() } catch { failure = failure ?? error }
        }
        if let run { _ = await run.result }
        run = nil; runObserver = nil
        if let clipboard {
            await clipboard.waitUntilSettled()
            presenter?.clipboard = nil; self.clipboard = nil
        }
        if let presentation {
            await presentation.waitUntilSettled()
            self.presentation = nil
        }
        if let resize { await resize.waitUntilSettled(); self.resize = nil }
        if let history { await history.waitUntilSettled(); self.history = nil }
        if let inspection = historyInspection { await inspection.waitUntilSettled(); historyInspection = nil }
        if let historyReturn { await historyReturn.value; self.historyReturn = nil; historyReturning = false }
        if let paletteChange { await paletteChange.value; self.paletteChange = nil }
        if let fontRevisionChange { await fontRevisionChange.value; self.fontRevisionChange = nil }
        if attached, let source = pump?.source {
            do {
                // A live scene removes the exact binding and joins its sealed
                // responder. If the scene is gone, the captured keyboard below
                // still owns cleanup; coordinator.retire checks for any orphan.
                try await focus?.detach(machineID: target.machineID, source: source)
                attached = false
            } catch { failure = failure ?? error }
        }
        // Keep the model available while keyboard cleanup finishes its canonical
        // transaction. Native-view retirement alone does not join that work.
        if let keyboard {
            await keyboard.waitUntilSuspended()
            self.keyboard = nil
        }
        let coordinator = connections.coordinator
        if let pump, !attached {
            let presenter = self.presenter
            do {
                try await pump.reconcileStoppedOutcomes { records, source in
                    guard let presenter else { throw TerminalInputError.unavailable }
                    try await presenter.receive(records, source: source)
                }
                guard let connection else { throw TerminalInputError.unavailable }
                try await connection.prepareInputSettlement(source: pump.source)
                try await coordinator.retire(pump.source)
                self.pump = nil
            } catch { failure = failure ?? error }
        }
        if let runtime, !attached {
            if retainEndedOutput, !retainedCaptureAttempted {
                retainedCaptureAttempted = true
                do {
                    let document = try await runtime.captureOutputDocument(source: runtime.source)
                    if retainEndedOutput {
                        retainedOutput = TerminalRetainedOutput(source: runtime.source,
                            destination: destination, document: document)
                    }
                } catch {
                    if retainEndedOutput { retainedOutputFailure = Self.describe(error) }
                }
            }
            // A mux pane's server says below whether it confirmed the input.
            if dropped, !attachedToMux, !inputDelivery.unanswered, await runtime.inputAwaitsAnswer() {
                inputDelivery.endedUnanswered()
            }
            do {
                try await runtime.close(); self.runtime = nil
                // This launch tells its own drops; the mark is for the next.
                unansweredInput.set(false)
            } catch { failure = failure ?? error }
        }
        if let model = unadoptedModel {
            do { try model.close(); unadoptedModel = nil }
            catch { failure = failure ?? error }
        }
        if let connection {
            // A cached seal survives source retirement. A protected-storage
            // error keeps this exact owner for retry, without recreating a shell
            // or retaining an otherwise settled coordinator source.
            do { try await connections.manager.close(connectionID: connection.id); self.connection = nil }
            catch { failure = failure ?? error }
            // Closing settles a mux pane's input: its server confirmed it or not.
            if dropped, connection.muxControl?.current.inputUnconfirmed == true, !inputDelivery.unanswered {
                inputDelivery.endedUnanswered()
            }
        }
        if let failure {
            cleanupFailure = Self.describe(failure)
            connections.retainForCleanup(self)
        } else {
            presenter?.view = nil; presenter?.onUnavailable = nil; presenter?.onInterruptedInput = nil
            presenter = nil
            settled = true
            connections.releaseCleanup(id)
        }
    }
}
