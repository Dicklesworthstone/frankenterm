import SwiftUI
import FrankenTermKit

/// Picks a machine's theme on this phone: the workspace's own themes, then
/// FrankenTerm's distinct themes, then every built-in scheme. A pick applies
/// at once, to a live terminal too, so the sheet stays open to compare.
struct MachineThemePicker: View {
    let machine: WorkspaceDocument.Machine
    let themes: MachineThemes
    let choose: (MachineThemeChoice) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var search = ""

    var body: some View {
        let current = themes.theme(for: machine)
        let own = themes.workspaceTheme(of: machine)
        let others = unique(themes.document.themes.filter { $0.id != machine.themeID && $0.name != own?.name && matches($0) })
        NavigationStack {
            List {
                if own.map(matches) == true || !others.isEmpty {
                    Section("This workspace") {
                        if let own, matches(own) {
                            row(own, title: "\(own.name) (exported for this machine)", current: current) {
                                choose(MachineThemeChoice(name: nil))
                            }
                        }
                        ForEach(others, id: \.id) { theme in
                            row(theme, current: current) { choose(MachineThemeChoice(name: theme.name)) }
                        }
                    }
                }
                Section("FrankenTerm") {
                    ForEach(themes.catalog.distinct.filter(matches), id: \.id) { theme in
                        row(theme, current: current) { choose(MachineThemeChoice(name: theme.name, catalog: true)) }
                    }
                }
                Section("Color schemes") {
                    ForEach(themes.catalog.themes.dropFirst(themes.catalog.distinctCount).filter(matches), id: \.id) { theme in
                        row(theme, current: current) { choose(MachineThemeChoice(name: theme.name, catalog: true)) }
                    }
                }
            }
            .searchable(text: $search, prompt: "Search \(themes.catalog.themes.count) themes")
            .navigationTitle(machine.name).navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
        }
        .presentationDetents([.medium, .large])
    }

    private func matches(_ theme: WorkspaceDocument.Theme) -> Bool {
        search.isEmpty || theme.name.localizedCaseInsensitiveContains(search)
    }

    /// Exported machines often share a theme name; list each name once.
    private func unique(_ list: [WorkspaceDocument.Theme]) -> [WorkspaceDocument.Theme] {
        var seen = Set<String>()
        return list.filter { seen.insert($0.name).inserted }
    }

    private func row(_ theme: WorkspaceDocument.Theme, title: String? = nil, current: WorkspaceDocument.Theme?,
                     pick: @escaping () -> Void) -> some View {
        let selected = current?.id == theme.id
        return Button(action: pick) {
            HStack(spacing: 12) {
                ThemeSwatch(theme: theme).frame(width: 64, height: 40)
                Text(verbatim: title ?? theme.name).foregroundStyle(.primary).lineLimit(2)
                Spacer()
                if selected { Image(systemName: "checkmark").foregroundStyle(.tint) }
            }
        }
        .accessibilityIdentifier("theme.\(theme.name)")
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

/// A theme at a glance: its background, accent, six ANSI colors and text.
struct ThemeSwatch: View {
    let theme: WorkspaceDocument.Theme
    var body: some View {
        RoundedRectangle(cornerRadius: 6).fill(Color(ftHex: theme.background))
            .overlay(alignment: .leading) {
                VStack(alignment: .leading, spacing: 4) {
                    Capsule().fill(Color(ftHex: theme.accent)).frame(width: 24, height: 4)
                    HStack(spacing: 2) {
                        ForEach(1..<7) { Circle().fill(Color(ftHex: theme.ansi[$0])).frame(width: 6, height: 6) }
                    }
                    Capsule().fill(Color(ftHex: theme.foreground)).frame(width: 32, height: 3)
                }.padding(6)
            }
            .overlay(RoundedRectangle(cornerRadius: 6).stroke(.secondary.opacity(0.3)))
            .accessibilityHidden(true)
    }
}
