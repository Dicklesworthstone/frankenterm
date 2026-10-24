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

/// Searches a machine's scrollback where it lives, on the machine's mux
/// server: the whole history of the pane on screen, or of every pane at once,
/// newest matches first. A match opens the rows around it; Show Pane puts
/// that pane on screen.
struct MuxSearchScreen: View {
    let session: TerminalSessionController
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase
    @State private var text = ""
    @State private var scope = Scope.pane
    @State private var caseSensitive = false
    @State private var regex = false
    @State private var typing: Task<Void, Never>?
    @State private var opened: MuxSearchHit?

    enum Scope: Hashable { case pane, machine }

    private var state: TerminalMuxControl.State? { session.muxState }

    var body: some View {
        NavigationStack {
            List {
                if let search = state?.search { results(search) } else { hint }
            }
            .listStyle(.plain)
            .searchable(text: $text, placement: .navigationBarDrawer(displayMode: .always),
                        prompt: scope == .pane ? "Search this pane's history" : "Search every pane's history")
            .searchScopes($scope) {
                Text("This Pane").tag(Scope.pane)
                Text("All Panes").tag(Scope.machine)
            }
            .textInputAutocapitalization(.never).autocorrectionDisabled()
            .onSubmit(of: .search) { run(after: .zero) }
            .onChange(of: text) { run(after: .milliseconds(350)) }
            .onChange(of: scope) { run(after: .zero) }
            .onChange(of: caseSensitive) { run(after: .zero) }
            .onChange(of: regex) { run(after: .zero) }
            .navigationTitle(session.target.machineName).navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Menu {
                        Toggle("Match Case", systemImage: "textformat", isOn: $caseSensitive)
                        Toggle("Regular Expression", systemImage: "asterisk", isOn: $regex)
                    } label: { Label("Options", systemImage: "line.3.horizontal.decrease.circle") }
                }
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
            .navigationDestination(item: $opened) { hit in
                MuxSearchContextScreen(session: session, hit: hit, pane: pane(hit.pane)) { dismiss() }
            }
        }
        .onAppear { session.beginMuxSearch() }
        .onDisappear { session.endMuxSearch(); typing?.cancel() }
        .onChange(of: scenePhase) { _, phase in
            if phase != .active { typing?.cancel(); session.endMuxSearch(); opened = nil; dismiss() }
        }
        .onChange(of: session.phase) { _, phase in
            if phase != .live { typing?.cancel(); session.endMuxSearch(); opened = nil; dismiss() }
        }
    }

    private var hint: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Searches scrollback on \(session.target.machineName), not just what this phone has drawn.",
                  systemImage: "magnifyingglass")
            Text("Pick This Pane or All Panes above the results.").foregroundStyle(.secondary)
            Text("Results have device limits and deadlines. Interrupted or limited searches are marked incomplete.")
                .foregroundStyle(.secondary)
        }
        .font(.callout).listRowSeparator(.hidden).padding(.vertical, 8)
    }

    /// Runs the search `delay` after the last change to it; a new one replaces it.
    private func run(after delay: Duration) {
        typing?.cancel()
        guard scenePhase == .active, session.phase == .live else { return }
        let query = text.trimmingCharacters(in: .whitespaces)
        guard !query.isEmpty, let control = session.muxControl, let state else {
            session.muxControl?.clearSearch(); return
        }
        let panes: [Int]
        switch scope {
        case .pane: panes = state.shown.map { [$0] } ?? []
        case .machine:
            // The pane on screen answers first.
            let all = state.panes.map(\.paneId)
            if let shown = state.shown { panes = [shown] + all.filter { $0 != shown } } else { panes = all }
        }
        let request = TerminalMuxControl.SearchQuery(text: query, caseSensitive: caseSensitive, regex: regex)
        typing = Task {
            if delay > .zero { try? await Task.sleep(for: delay) }
            guard !Task.isCancelled, scenePhase == .active, session.phase == .live,
                  session.muxControl === control else { return }
            control.search(request, panes: panes)
        }
    }

    private func pane(_ id: Int) -> TerminalMuxPane? { state?.panes.first { $0.paneId == id } }

    @ViewBuilder private func results(_ search: TerminalMuxControl.Search) -> some View {
        let answered = search.panes.compactMap { id in search.results[id].map { (id, $0) } }
        let total = answered.reduce(0) { $0 + $1.1.matches.count }
        Section {
            HStack(spacing: 8) {
                if !search.pending.isEmpty { ProgressView().controlSize(.small) }
                Text(summary(search, total: total)).font(.caption).foregroundStyle(.secondary)
                if !search.pending.isEmpty {
                    Spacer()
                    Button("Stop Search") { typing?.cancel(); session.muxControl?.cancelSearch(search.id) }
                        .accessibilityIdentifier("terminal.mux.search.stop")
                }
            }
            if let issue = search.issue {
                Text(verbatim: issue).font(.callout).foregroundStyle(.orange)
            }
        }
        ForEach(answered.filter { !$0.1.matches.isEmpty || $0.1.error != nil || search.panes.count == 1 }, id: \.0) { id, result in
            Section {
                if let error = result.error {
                    Label(error, systemImage: "exclamationmark.triangle").font(.callout).foregroundStyle(.orange)
                } else if result.matches.isEmpty {
                    Text("No matches in the last \(result.searchedRows.formatted()) rows.").font(.callout).foregroundStyle(.secondary)
                }
                ForEach(result.matches) { match in
                    Button { opened = MuxSearchHit(searchID: search.id, pane: id, match: match, query: search.query) } label: {
                        MuxSearchMatchRow(match: match, query: search.query)
                    }
                }
            } header: {
                MuxPaneHeader(pane: pane(id), count: result.matches.count, shown: id == state?.shown)
            }
        }
    }

    private func summary(_ search: TerminalMuxControl.Search, total: Int) -> String {
        let panes = search.panes.count == 1 ? "this pane" : "\(search.panes.count) panes"
        if !search.pending.isEmpty {
            return "Searching \(panes)… \(total) match\(total == 1 ? "" : "es") so far"
        }
        if search.issue != nil || search.results.values.contains(where: { $0.error != nil }) {
            return "\(total) matches received. This search is incomplete."
        }
        return total == 0 ? "No matches in the searched rows of \(panes)." : "\(total) match\(total == 1 ? "" : "es") received from \(panes), newest first"
    }
}

/// One match opened from the results.
struct MuxSearchHit: Hashable, Identifiable {
    let searchID: UUID
    let pane: Int
    let match: TerminalMuxSearchMatch
    let query: TerminalMuxControl.SearchQuery
    var id: String { "\(searchID):\(pane):\(match.id)" }
    static func == (a: Self, b: Self) -> Bool { a.id == b.id }
    func hash(into hasher: inout Hasher) { hasher.combine(id) }
}

private struct MuxPaneHeader: View {
    let pane: TerminalMuxPane?
    let count: Int
    let shown: Bool
    var body: some View {
        HStack(spacing: 6) {
            if shown { Image(systemName: "eye").accessibilityLabel("On screen") }
            Text(verbatim: pane.map(Self.name) ?? "Closed pane").lineLimit(1)
            Spacer()
            Text("\(count)").monospacedDigit()
        }
    }
    /// The project directory, then the program's title.
    static func name(_ pane: TerminalMuxPane) -> String {
        let directory = pane.workingDir.flatMap { URL(string: $0)?.lastPathComponent }.flatMap { $0.isEmpty || $0 == "/" ? nil : $0 }
        let title = pane.title.trimmingCharacters(in: .whitespaces)
        switch (directory, title.isEmpty) {
        case let (directory?, false) where directory != title: return "\(directory) · \(title)"
        case let (directory?, _): return directory
        default: return title.isEmpty ? "Pane \(pane.paneId)" : title
        }
    }
}

/// A match's first row with the matched cells marked.
private struct MuxSearchMatchRow: View {
    let match: TerminalMuxSearchMatch
    let query: TerminalMuxControl.SearchQuery
    var body: some View {
        Text(MuxSearchHighlight.marked(match))
            .font(.system(.footnote, design: .monospaced)).lineLimit(2)
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}

enum MuxSearchHighlight {
    /// The match's first row with the match marked: exactly where its
    /// offsets say, else by cells (ASCII rows only).
    static func marked(_ match: TerminalMuxSearchMatch) -> AttributedString {
        if let range = match.matchedRange { return marked(match.text, range: range) }
        return marked(match.text.split(separator: "\n", omittingEmptySubsequences: false).first.map(String.init) ?? "",
                      from: match.startColumn, to: match.endRow == match.row ? match.endColumn : nil)
    }

    /// The row of `text` where `range` starts, with `range` marked (to the
    /// row's end, for a match running onto the next rows).
    static func marked(_ text: String, range: Range<String.Index>) -> AttributedString {
        let rowEnd = text[range.lowerBound...].firstIndex(of: "\n") ?? text.endIndex
        let rowStart = text[..<range.lowerBound].lastIndex(of: "\n").map { text.index(after: $0) } ?? text.startIndex
        let upper = min(range.upperBound, rowEnd)
        let before = text[rowStart..<range.lowerBound]
        var marked = AttributedString(before.count > 24 ? "…" + String(before.suffix(20)) : String(before))
        var hit = AttributedString(String(text[range.lowerBound..<upper]))
        hit.backgroundColor = .yellow.opacity(0.45)
        hit.font = .system(.footnote, design: .monospaced).bold()
        marked += hit
        marked += AttributedString(String(text[upper..<rowEnd]))
        return marked
    }

    /// `line` with cells `start..<end` marked (to the line's end without
    /// `end`), and the long lead-in before a far-right match shortened.
    static func marked(_ line: String, from start: Int, to end: Int?) -> AttributedString {
        // The wire supplies cell coordinates but only plain text, not a cell
        // map. Swift Character indices are not terminal columns for wide or
        // combining glyphs. Highlight the matching row rather than inventing a
        // substring location in that case; exact spans require canonical cells.
        guard line.utf8.allSatisfy({ (32...126).contains($0) }) else {
            var text = AttributedString(line)
            text.backgroundColor = .yellow.opacity(0.15)
            return text
        }
        let characters = Array(line)
        let lower = min(max(start, 0), characters.count)
        let upper = min(max(end ?? characters.count, lower), characters.count)
        let lead = lower > 24 ? "…" + String(characters[(lower - 20)..<lower]) : String(characters[..<lower])
        var text = AttributedString(lead)
        var hit = AttributedString(String(characters[lower..<upper]))
        hit.backgroundColor = .yellow.opacity(0.45)
        hit.font = .system(.footnote, design: .monospaced).bold()
        text += hit
        text += AttributedString(String(characters[upper...]))
        return text
    }
}

/// The rows around one match, read from the server, with the match's row
/// marked. Copy takes them all; Show Pane puts the pane on screen.
private struct MuxSearchContextScreen: View {
    let session: TerminalSessionController
    let hit: MuxSearchHit
    let pane: TerminalMuxPane?
    let close: () -> Void
    @State private var copied = false

    private var context: TerminalMuxControl.Context? {
        session.muxState?.context.flatMap { $0.pane == hit.pane && $0.row == hit.match.row ? $0 : nil }
    }

    private var hitIsCurrent: Bool {
        guard session.phase == .live, let state = session.muxControl?.current,
              state.search?.id == hit.searchID, state.panes.contains(where: { $0.paneId == hit.pane }) else { return false }
        return state.search?.results[hit.pane]?.matches.contains(hit.match) == true
    }

    var body: some View {
        ScrollViewReader { reader in
            ScrollView([.vertical, .horizontal]) {
                LazyVStack(alignment: .leading, spacing: 0) {
                    if !hitIsCurrent {
                        Text("This result is no longer current. Return to the search and try again.").padding()
                    } else if let error = context?.error {
                        Text(verbatim: error).padding().foregroundStyle(.orange)
                    } else if let lines = context?.lines {
                        if lines.isEmpty { Text("These rows are no longer on the machine.").padding() }
                        ForEach(lines) { line in
                            row(line).id(line.row)
                        }
                    } else {
                        ProgressView("Reading the rows around it…").padding()
                    }
                }
                .padding(.vertical, 8)
            }
            .onChange(of: context?.lines?.count) {
                reader.scrollTo(hit.match.row, anchor: .center)
            }
        }
        .navigationTitle(pane.map(MuxPaneHeader.name) ?? "Pane \(hit.pane)").navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItemGroup(placement: .bottomBar) {
                Button(copied ? "Copied" : "Copy", systemImage: copied ? "checkmark" : "doc.on.doc") { copy() }
                    .disabled(!hitIsCurrent || (context?.lines?.isEmpty ?? true))
                Spacer()
                Button("Show Pane", systemImage: "rectangle.portrait.and.arrow.right") {
                    guard hitIsCurrent else { return }
                    session.endMuxSearch(resumingView: false); session.showPane(hit.pane); close()
                }.buttonStyle(.borderedProminent).disabled(!hitIsCurrent)
            }
        }
        .onAppear { session.muxControl?.readContext(pane: hit.pane, row: hit.match.row, searchID: hit.searchID) }
        .onDisappear { session.muxControl?.clearContext() }
    }

    @ViewBuilder private func row(_ line: TerminalMuxContextLine) -> some View {
        let matched = line.row == hit.match.row
        Group {
            if matched, let range = hit.match.matchedRange,
               hit.match.text.split(separator: "\n", omittingEmptySubsequences: false).first.map(String.init) == line.text {
                // The match's text began with this very row: its offsets apply.
                Text(MuxSearchHighlight.marked(hit.match.text, range: range))
            } else if matched {
                Text(MuxSearchHighlight.marked(line.text, from: hit.match.startColumn,
                                               to: hit.match.endRow == hit.match.row ? hit.match.endColumn : nil))
            } else {
                Text(verbatim: line.text.isEmpty ? " " : line.text)
            }
        }
        .font(.system(.footnote, design: .monospaced))
        .textSelection(.enabled)
        .padding(.horizontal, 12).padding(.vertical, 1)
        .frame(minWidth: 0, alignment: .leading)
        .background(matched ? Color.accentColor.opacity(0.15) : .clear)
    }

    /// The rows as plain text; like other copies, it stays on this device and
    /// expires after two minutes.
    private func copy() {
        guard hitIsCurrent, let lines = context?.lines, !lines.isEmpty else { return }
        UIPasteboard.general.setItems([["public.utf8-plain-text": lines.map(\.text).joined(separator: "\n")]],
            options: [.localOnly: true, .expirationDate: Date().addingTimeInterval(120)])
        copied = true
    }
}
