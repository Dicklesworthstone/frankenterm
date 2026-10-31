import SwiftUI
import UIKit
import Observation
import UniformTypeIdentifiers
import FrankenTermSSH
import FrankenTermSSHIntegration

/// A file browser for an independently reviewed SSH route. No terminal,
/// terminal source, PTY or shell-creation consent is needed to present it.
@MainActor
struct WorkspaceFilesScreen: View {
    @Bindable var controller: WorkspaceFilesController
    @State private var native: WorkspaceFilesNativeState
    @State private var path = ""
    @State private var saveFile: TerminalFileExportLease?
    @State private var shareFile: TerminalFileExportLease?
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase

    init(controller: WorkspaceFilesController) {
        self.controller = controller
        _native = State(initialValue: WorkspaceFilesNativeState(controller: controller))
    }

    var body: some View {
        Group {
            Group {
                if scenePhase == .active { browser }
                else {
                    ContentUnavailableView("Remote Files Paused", systemImage: "lock",
                        description: Text("Return to FrankenTerm to view files."))
                }
            }
            .navigationTitle("Remote Files").navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Done") { native.close(); dismiss() }
                        .accessibilityIdentifier("workspace.files.done")
                }
            }
        }
        .background {
            if let request = native.importRequest {
                RemoteFilesImportPicker(requestID: request.id,
                    selected: { id, result in native.receiveImport(result, requestID: id) },
                    cancelled: { id in native.cancelImport(requestID: id) })
                    .id(request.id)
            }
        }
        .sheet(item: $saveFile) { file in TerminalFileSavePicker(file: file) }
        .popover(item: $shareFile, attachmentAnchor: .rect(.bounds), arrowEdge: .bottom) { file in
            TerminalFileShareSheet(file: file).presentationCompactAdaptation(.sheet)
        }
        .task {
            native.setActive(scenePhase == .active)
            path = controller.listing?.path ?? ""
            if controller.listing == nil, !controller.isClosed, !controller.isBusy,
               scenePhase == .active { await controller.refresh() }
        }
        .onChange(of: controller.listingID) { _, _ in
            path = controller.listing?.path ?? ""
            native.directoryChanged()
        }
        .onChange(of: scenePhase) { _, phase in
            native.setActive(phase == .active)
            // Files providers and Face ID can briefly make the app inactive.
            // Only backgrounding retires this visit and its picker tickets.
            if phase == .background {
                saveFile = nil; shareFile = nil
                native.close()
            }
        }
        .onDisappear {
            saveFile = nil; shareFile = nil
            native.close()
        }
    }

    private var browser: some View {
        List {
            Section("Verified destination") {
                LabeledContent("Machine") { Text(verbatim: shown(controller.machineName)) }
                LabeledContent("Account") { Text(verbatim: shown(controller.account)) }
                LabeledContent("Host") { Text(verbatim: shown(controller.host)) }
                LabeledContent("Port", value: String(controller.port))
                LabeledContent("Route") { Text(verbatim: shown(controller.scope.routeID)) }
                Text("This separate file connection opens no terminal or shell. Transfers run while this browser is open in the foreground.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            if let message = native.message {
                Section { Text(verbatim: message).foregroundStyle(.orange) }
            }
            if let failure = controller.failure {
                Section(failure.wasCancelled ? "Operation Cancelled" : "File Operation Stopped") {
                    Text(verbatim: failure.message)
                    Text("Confirmed transfer: \(bytes(failure.transferredBytes)).")
                        .font(.caption).foregroundStyle(.secondary)
                    if failure.remoteFileMayRemain {
                        Label("A remote file may remain", systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.orange)
                        if let path = failure.remotePath {
                            Text(verbatim: shown(path)).font(.body.monospaced()).textSelection(.enabled)
                        }
                        Text("Inspect the destination before choosing a new filename. Nothing will be retried or overwritten automatically.")
                            .font(.caption)
                    }
                }.accessibilityIdentifier("workspace.files.failure")
            }
            if native.isStaging {
                Section("Preparing Upload Review") {
                    progress(done: native.stagedBytes, total: native.stagedTotal)
                    Text("Preparing a protected copy of the selected bytes.")
                        .font(.caption).foregroundStyle(.secondary)
                    Button("Cancel Preparation", role: .cancel) { native.cancelPreparation() }
                }
            } else if let activity = controller.activity {
                Section(activityTitle(activity)) {
                    if let progress = controller.progress {
                        self.progress(done: progress.transferredBytes, total: progress.totalBytes)
                    } else { ProgressView("Reading directory…") }
                    Button("Cancel", role: .cancel) { native.cancelOperation() }
                        .accessibilityIdentifier("workspace.files.cancel")
                }
            } else if controller.isClosed {
                Section {
                    Label("File Connection Closed", systemImage: "network.slash")
                    Text("Close this browser and choose Remote Files again to make a new connection. An interrupted transfer will not resume.")
                        .font(.callout).foregroundStyle(.secondary)
                }
            } else if native.hasUpload {
                uploadReview
            } else if let review = controller.review {
                downloadReview(review)
            } else if let listing = controller.listing {
                directory(listing, listingID: controller.listingID)
            }
            if let completion = controller.completion { completed(completion) }
        }
        .accessibilityIdentifier("workspace.files.browser")
    }

    private func directory(_ listing: NativeSFTPDirectoryListing, listingID: UUID) -> some View {
        Group {
            Section("Directory") {
                Text(verbatim: shown(listing.path)).font(.body.monospaced()).textSelection(.enabled)
                TextField("Go to an absolute directory", text: $path)
                    .textInputAutocapitalization(.never).autocorrectionDisabled()
                    .submitLabel(.go).onSubmit { native.openPath(path, listingID: listingID) }
                    .disabled(!native.canBrowse)
                    .accessibilityIdentifier("workspace.files.path")
                HStack {
                    Button("Up", systemImage: "arrow.up") { native.goUp(listingID: listingID) }
                        .disabled(!native.canBrowse || listing.path == "/")
                    Spacer()
                    Button("Go") { native.openPath(path, listingID: listingID) }.disabled(!native.canBrowse)
                    Button("Refresh", systemImage: "arrow.clockwise") { native.refresh(listingID: listingID) }
                        .disabled(!native.canBrowse)
                }.buttonStyle(.borderless)
                Button("Upload from Files", systemImage: "square.and.arrow.up") { native.beginImport(listingID: listingID) }
                    .disabled(!native.canBrowse).accessibilityIdentifier("workspace.files.upload.pick")
                Text("Regular files up to 256 MiB. Uploads create a new filename. Symbolic links and special files are not opened.")
                    .font(.caption).foregroundStyle(.secondary)
            }
            Section("Contents") {
                if listing.entries.isEmpty { Text("This directory is empty.").foregroundStyle(.secondary) }
                ForEach(listing.entries) { entry in
                    switch entry.attributes.kind {
                    case .directory:
                        Button {
                            native.openDirectory(entry.id, listingID: listingID)
                        } label: {
                            Label { Text(verbatim: shown(entry.name)) } icon: { Image(systemName: "folder") }
                        }.disabled(!native.canBrowse)
                    case .regularFile:
                        Button {
                            native.prepareDownload(entry.id, listingID: listingID)
                        } label: {
                            HStack {
                                Label { Text(verbatim: shown(entry.name)) } icon: { Image(systemName: "doc") }
                                Spacer()
                                if let size = entry.attributes.size {
                                    Text(bytes(size)).font(.caption).foregroundStyle(.secondary)
                                }
                                Image(systemName: "arrow.down.circle")
                            }
                        }.disabled(!native.canBrowse)
                    case .symbolicLink, .other, .unknown:
                        VStack(alignment: .leading, spacing: 3) {
                            Label { Text(verbatim: shown(entry.name)) } icon: {
                                Image(systemName: entry.attributes.kind == .symbolicLink ? "link" : "doc.questionmark")
                            }
                            Text(entry.attributes.kind == .symbolicLink ? "Symbolic link · not opened" : "Unsupported file type")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
                if listing.isTruncated {
                    Text("Showing the first \(listing.entries.count) entries. More exist on the server. Enter a known directory path to browse another location.")
                        .font(.footnote).foregroundStyle(.secondary)
                        .accessibilityIdentifier("workspace.files.truncated")
                }
            }
        }
    }

    private var uploadReview: some View {
        Section("Review New Remote File") {
            if let name = native.sourceName { LabeledContent("Selected file") { Text(verbatim: shown(name)) } }
            if let count = native.sourceBytes { LabeledContent("Size", value: bytes(count)) }
            TextField("New remote filename", text: Binding(get: { native.uploadName }, set: { native.editUploadName($0) }))
                .textInputAutocapitalization(.never).autocorrectionDisabled()
                .disabled(native.hasWork).accessibilityIdentifier("workspace.files.upload.name")
            if let review = controller.review, review.direction == .upload {
                Text(verbatim: shown(review.remotePath)).font(.body.monospaced()).textSelection(.enabled)
                Button("Create New File", systemImage: "plus") { native.confirm(review.id) }
                    .buttonStyle(.borderedProminent).disabled(!native.canConfirm)
                    .accessibilityIdentifier("workspace.files.upload.create")
            }
            Text("Only the protected copy you selected will be uploaded. An existing remote file is never replaced. Cancellation can leave an incomplete remote file.")
                .font(.caption).foregroundStyle(.secondary)
            Button("Discard Upload", role: .cancel) { native.discardReview() }.disabled(native.hasWork)
        }
    }

    private func downloadReview(_ review: WorkspaceFilesTransferReview) -> some View {
        Section("Review Download") {
            Text(verbatim: shown(review.remotePath)).font(.body.monospaced()).textSelection(.enabled)
            if let count = review.expectedBytes { LabeledContent("Size", value: bytes(count)) }
            else { Text("The server did not report a size. The 256 MiB limit still applies.").font(.caption) }
            Text("The complete file will be available for Save to Files or Share after the download is confirmed. Partial files cannot be exported.")
                .font(.caption).foregroundStyle(.secondary)
            Button("Download", systemImage: "arrow.down.circle") { native.confirm(review.id) }
                .buttonStyle(.borderedProminent).disabled(!native.canConfirm)
                .accessibilityIdentifier("workspace.files.download.confirm")
            Button("Cancel Review", role: .cancel) { native.discardReview() }.disabled(native.hasWork)
        }
    }

    private func completed(_ completion: WorkspaceFilesCompletion) -> some View {
        Section(completion.receipt.direction == .download ? "Download Complete" : "Remote File Created") {
            Text(verbatim: shown(completion.receipt.remotePath)).font(.body.monospaced()).textSelection(.enabled)
            Text(bytes(completion.receipt.transferredBytes)).foregroundStyle(.secondary)
            if completion.downloadURL != nil {
                Button("Save to Files", systemImage: "folder") {
                    guard native.canExport else { return }
                    saveFile = TerminalFileExportLease(completion: completion)
                }.disabled(!native.canExport).accessibilityIdentifier("workspace.files.save")
                Button("Share", systemImage: "square.and.arrow.up") {
                    guard native.canExport else { return }
                    shareFile = TerminalFileExportLease(completion: completion)
                }.disabled(!native.canExport).accessibilityIdentifier("workspace.files.share")
                Text("Save or share this completed download before closing the browser.")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }.accessibilityIdentifier("workspace.files.completed")
    }

    @ViewBuilder private func progress(done: UInt64, total: UInt64?) -> some View {
        if let total, total > 0 {
            ProgressView(value: Double(min(done, total)), total: Double(total))
            Text("\(bytes(done)) of \(bytes(total))").font(.caption).monospacedDigit()
        } else {
            ProgressView()
            Text(bytes(done)).font(.caption).monospacedDigit()
        }
    }

    private func activityTitle(_ activity: WorkspaceFilesActivity) -> String {
        switch activity { case .listing: "Directory"; case .upload: "Uploading"; case .download: "Downloading" }
    }
    private func bytes(_ count: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: count), countStyle: .file)
    }
    /// Make control and directional-format characters visible without changing
    /// the byte identity passed to the file controller. Escape backslashes too.
    private func shown(_ value: String) -> String {
        value.unicodeScalars.map { scalar in
            if scalar == "\\" { return "\\\\" }
            if scalar.properties.generalCategory == .control || scalar.properties.generalCategory == .format {
                return "\\u{\(String(scalar.value, radix: 16, uppercase: true))}"
            }
            return String(scalar)
        }.joined()
    }
}

/// Native provider staging and picker ownership. The portable file controller
/// owns every network action; this object holds only presentation-bound work.
@MainActor @Observable
final class WorkspaceFilesNativeState {
    struct ImportRequest: Identifiable { let id = UUID(); let listingID: UUID }
    let controller: WorkspaceFilesController
    private(set) var importRequest: ImportRequest?
    private(set) var uploadName = ""
    private(set) var sourceName: String?
    private(set) var sourceBytes: UInt64?
    private(set) var stagedBytes: UInt64 = 0
    private(set) var stagedTotal: UInt64?
    private(set) var isStaging = false
    private(set) var hasWork = false
    private(set) var message: String?
    private(set) var active = true
    private var retired = false
    private var pendingImport: (request: ImportRequest, source: URL)?
    @ObservationIgnored private var upload: WorkspaceFilesUploadSource?
    @ObservationIgnored private var uploadListingID: UUID?
    @ObservationIgnored private var work: (id: UUID, task: Task<Void, Never>)?

    init(controller: WorkspaceFilesController) { self.controller = controller }
    var hasUpload: Bool { upload != nil }
    var canBrowse: Bool {
        active && !retired && !controller.isClosed && !controller.isBusy && !hasWork
            && upload == nil && controller.review == nil && importRequest == nil && pendingImport == nil
    }
    var canConfirm: Bool { active && !retired && !controller.isClosed && !controller.isBusy && !hasWork && controller.review != nil }
    var canExport: Bool { active && !retired && !controller.isBusy && !hasWork && importRequest == nil }

    func setActive(_ value: Bool) {
        active = value
        if value, !retired, let pending = pendingImport {
            pendingImport = nil
            prepareSelection(pending.source, request: pending.request)
        }
    }

    func beginImport(listingID: UUID) {
        guard canBrowse, controller.listing != nil, controller.listingID == listingID else { return }
        message = nil; importRequest = .init(listingID: listingID)
    }
    func cancelImport(requestID: UUID) {
        if importRequest?.id == requestID { importRequest = nil }
    }
    func receiveImport(_ result: Result<[URL], any Error>, requestID: UUID) {
        guard !retired, let request = importRequest, request.id == requestID else { return }
        importRequest = nil
        guard !controller.isClosed, !controller.isBusy, !hasWork, upload == nil,
              controller.review == nil, request.listingID == controller.listingID else { return }
        let source: URL
        switch result {
        case let .success(urls):
            guard urls.count == 1, let selected = urls.first else { message = "Choose one regular file to upload."; return }
            source = selected
        case let .failure(error):
            if (error as NSError).code != NSUserCancelledError { message = "The selected file could not be opened from Files." }
            return
        }
        if !active {
            // The matching system callback is consumed even during a transient
            // interruption. Its exact URL/ticket may prepare on foregrounding;
            // it cannot borrow another picker or start a remote operation.
            pendingImport = (request, source)
            return
        }
        prepareSelection(source, request: request)
    }

    private func prepareSelection(_ source: URL, request: ImportRequest) {
        guard canBrowse, request.listingID == controller.listingID else { return }
        isStaging = true; stagedBytes = 0; stagedTotal = nil; message = nil
        let id = UUID()
        let task = Task { [weak self] in
            guard let self else { return }
            defer { self.finishWork(id) }
            do {
                try Task.checkCancellation()
                guard !self.retired, self.controller.listingID == request.listingID else { return }
                if !self.active {
                    self.pendingImport = (request, source)
                    return
                }
                let copying = Task.detached(priority: .userInitiated) { [weak self] in
                    try await TerminalFileStaging.copy(source) { bytes, total in
                        await self?.stagingProgress(bytes, total: total, id: id)
                    }
                }
                let file = try await withTaskCancellationHandler { try await copying.value } onCancel: { copying.cancel() }
                try Task.checkCancellation()
                guard !self.retired, self.controller.listingID == request.listingID, !self.controller.isClosed else { return }
                self.upload = try WorkspaceFilesUploadSource(takingOwnershipOfStagedFile: file.storage.url, name: file.name)
                self.uploadListingID = request.listingID
                self.sourceName = file.name; self.sourceBytes = file.bytes; self.uploadName = file.name
                self.updateUploadReview()
            } catch {
                if !self.retired, !(error is CancellationError) { self.message = error.localizedDescription }
            }
        }
        work = (id, task); hasWork = true
    }

    func editUploadName(_ value: String) {
        guard !retired, active, !hasWork, !controller.isBusy, upload != nil else { return }
        // Editing remains bounded even before the portable path validator runs.
        guard value.utf8.count <= 1_024 else { return }
        uploadName = value
        updateUploadReview()
    }
    private func updateUploadReview() {
        controller.dismissReview()
        guard let upload, let listingID = uploadListingID, !retired, !controller.isClosed else { return }
        do {
            try controller.prepareUpload(source: upload, remoteName: uploadName, listingID: listingID)
            message = nil
        } catch { message = error.localizedDescription }
    }

    func prepareDownload(_ entryID: Data, listingID: UUID) {
        guard canBrowse else { return }
        do { try controller.prepareDownload(entryID: entryID, listingID: listingID); message = nil }
        catch { message = error.localizedDescription }
    }
    func confirm(_ reviewID: UUID) {
        guard canConfirm, controller.review?.id == reviewID else { return }
        let id = UUID(), controller = controller, retainedUpload = upload
        let task = Task { [weak self] in
            guard let self else { return }
            defer { self.finishWork(id) }
            guard !Task.isCancelled, !self.retired, self.active, self.controller.review?.id == reviewID else { return }
            await controller.confirm(reviewID: reviewID)
            withExtendedLifetime(retainedUpload) {}
            self.clearUpload()
        }
        work = (id, task); hasWork = true
    }
    func discardReview() {
        guard !hasWork, !controller.isBusy else { return }
        controller.dismissReview(); clearUpload(); message = nil
    }
    func directoryChanged() {
        importRequest = nil; pendingImport = nil
        if let listingID = uploadListingID, listingID != controller.listingID {
            controller.dismissReview(); clearUpload()
        }
    }

    func refresh(listingID: UUID) {
        guard canBrowse, controller.listingID == listingID else { return }
        runListing { await self.controller.refresh() }
    }
    func openPath(_ path: String, listingID: UUID) {
        guard canBrowse, controller.listingID == listingID else { return }
        runListing { await self.controller.openPath(path) }
    }
    func goUp(listingID: UUID) {
        guard canBrowse else { return }
        runListing { await self.controller.goUp(listingID: listingID) }
    }
    func openDirectory(_ entryID: Data, listingID: UUID) {
        guard canBrowse else { return }
        runListing { await self.controller.openDirectory(entryID: entryID, listingID: listingID) }
    }
    private func runListing(_ operation: @escaping @MainActor () async -> Void) {
        let id = UUID(); message = nil
        let task = Task { [weak self] in
            guard let self else { return }
            defer { self.finishWork(id) }
            guard !Task.isCancelled, !self.retired, self.active else { return }
            await operation()
        }
        work = (id, task); hasWork = true
    }

    func cancelPreparation() { if isStaging { work?.task.cancel() } }
    func cancelOperation() {
        guard !retired else { return }
        work?.task.cancel()
        Task { await controller.cancel() }
    }
    func close() {
        guard !retired else { return }
        retired = true; importRequest = nil; pendingImport = nil; clearUpload()
        work?.task.cancel()
        Task { await controller.close() }
    }
    private func clearUpload() { upload = nil; uploadListingID = nil; sourceName = nil; sourceBytes = nil; uploadName = "" }
    private func finishWork(_ id: UUID) {
        guard work?.id == id else { return }
        work = nil; hasWork = false; isStaging = false
    }
    private func stagingProgress(_ bytes: UInt64, total: UInt64?, id: UUID) {
        guard !retired, work?.id == id else { return }
        stagedBytes = max(stagedBytes, bytes); stagedTotal = total
    }
}

/// Each native Files presentation captures one immutable ticket. A callback
/// from a dismissed picker cannot borrow a later presentation's request ID.
@MainActor
struct RemoteFilesImportPicker: View {
    let requestID: UUID
    let selected: @MainActor (UUID, Result<[URL], any Error>) -> Void
    let cancelled: @MainActor (UUID) -> Void
    @State private var presented = false
    var body: some View {
        Color.clear.frame(width: 0, height: 0)
            .fileImporter(isPresented: $presented, allowedContentTypes: [.item], allowsMultipleSelection: false) { [requestID, selected] result in
                Task { @MainActor in selected(requestID, result) }
            } onCancellation: { [requestID, cancelled] in
                Task { @MainActor in cancelled(requestID) }
            }
            .task { presented = true }
    }
}
