import SwiftUI
import UIKit
import FrankenTermKit

/// Volatile, bounded received output. It carries no model or SSH authority and
/// is discarded when its scene closes or its machine starts a new session.
struct TerminalRetainedOutput: Identifiable {
    let id = UUID()
    let source: TerminalSource
    let destination: String
    let document: TerminalCopyDocument
}

struct TerminalHistoryScreen: View {
    @Bindable var inspection: TerminalHistoryInspection
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            ScrollViewReader { reader in
                ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text(verbatim: inspection.destination).font(.caption).textSelection(.disabled)
                Text("Search output retained on this device. Literal, case-sensitive, one physical screen row at a time; concealed text is excluded.")
                    .font(.caption).foregroundStyle(.secondary)
                HStack {
                    TextField("Find in retained output", text: $inspection.query)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                        .submitLabel(.search).onSubmit { inspection.search() }
                        .accessibilityIdentifier("terminal.history.query")
                    Button("Search", systemImage: "magnifyingglass") { inspection.search() }
                        .disabled(inspection.isBusy || inspection.isClosing || inspection.query.isEmpty)
                }
                if inspection.isBusy {
                    HStack {
                        ProgressView("Reading retained output…")
                        Button("Cancel") { inspection.cancelSearch() }.disabled(inspection.isClosing)
                    }
                }
                if let failure = inspection.failure {
                    Text(verbatim: failure).font(.callout).foregroundStyle(.red)
                }
                if let result = inspection.result {
                    Text(summary(result)).font(.caption).foregroundStyle(.secondary)
                    if result.matches.isEmpty { Text("No matches in the scanned rows.").font(.callout) }
                    LazyVStack(alignment: .leading, spacing: 12) {
                        ForEach(result.matches) { match in
                        Button { inspection.reveal(match) } label: {
                            VStack(alignment: .leading, spacing: 4) {
                                Text("Row \(match.row) · Columns \(match.columns.lowerBound + 1)–\(match.columns.upperBound)")
                                    .font(.caption).foregroundStyle(.secondary)
                                Text(verbatim: match.excerpt).font(.system(.body, design: .monospaced))
                                    .lineLimit(3)
                            }
                        }.disabled(inspection.isBusy || inspection.isClosing)
                            Divider()
                        }
                    }
                }
                if let preview = inspection.preview, let previewID = inspection.previewID {
                    Text("Selected received viewport. New output can still arrive; screen-row breaks are preserved.")
                        .font(.caption).foregroundStyle(.secondary)
                        .id("selected-preview")
                    TerminalReadOnlyOutput(document: preview).id(previewID).frame(height: 220)
                    Button("Show in Terminal", systemImage: "terminal") {
                        Task { await inspection.close(restoreViewport: false) }
                    }.buttonStyle(.borderedProminent).disabled(inspection.isBusy || inspection.isClosing)
                }
            }.padding()
                }
                .onChange(of: inspection.previewID) { _, previewID in
                    if previewID != nil { withAnimation { reader.scrollTo("selected-preview", anchor: .top) } }
                }
            }
                .navigationTitle("Search Output").navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Done") { Task { await inspection.close() } }.disabled(inspection.isClosing)
                    }
                }
                .overlay { if inspection.isClosing { ProgressView("Returning to terminal…").padding().background(.regularMaterial) } }
        }
        .onChange(of: inspection.isPresented, initial: true) { _, presented in if !presented { dismiss() } }
        .onDisappear { let inspected = inspection; Task { await inspected.close() } }
    }

    private func summary(_ result: TerminalHistorySearchResult<TerminalSource>) -> String {
        let scope = "\(result.matches.count) matches in \(result.scannedRows) scanned rows."
        switch result.completion {
        case .complete: return scope + " Retained range at search start: \(result.firstRow)…\(result.lastRow)."
        case .limitReached: return scope + " Search stopped at its local row, byte, page or match limit. Older output may have more matches."
        case .historyChanged: return scope + " History changed while searching. Search again for the current retained range."
        }
    }
}

struct TerminalRetainedOutputScreen: View {
    let output: TerminalRetainedOutput
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        NavigationStack {
            ScrollView {
            VStack(alignment: .leading, spacing: 12) {
                Text(verbatim: output.destination).font(.caption)
                Text("The last received live viewport, kept only in memory. This session is closed. This is not the full transcript; later remote output may be missing. Screen-row breaks are preserved.")
                    .font(.callout).foregroundStyle(.secondary)
                Text("Select text to copy. Copies stay on this device and expire after two minutes.")
                    .font(.caption).foregroundStyle(.secondary)
                TerminalReadOnlyOutput(document: output.document).frame(minHeight: 300)
            }.padding()
            }
                .navigationTitle("Retained Output").navigationBarTitleDisplayMode(.inline)
                .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
        }
        .onChange(of: scenePhase, initial: true) { _, phase in if phase != .active { dismiss() } }
    }
}

/// Reuse the same grapheme-safe, local-only, expiring copy path as Select Output.
/// Dismantling immediately releases plaintext and disables selection actions.
private struct TerminalReadOnlyOutput: UIViewRepresentable {
    let document: TerminalCopyDocument
    func makeUIView(context: Context) -> NativeTerminalCopyTextView {
        NativeTerminalCopyTextView(document: document)
    }
    func updateUIView(_ view: NativeTerminalCopyTextView, context: Context) {}
    static func dismantleUIView(_ view: NativeTerminalCopyTextView, coordinator: ()) { view.clearSnapshot() }
}
