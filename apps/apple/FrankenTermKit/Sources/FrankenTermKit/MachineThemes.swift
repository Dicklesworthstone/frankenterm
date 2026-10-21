import Foundation

/// Every theme the phone can offer a machine, bundled with the app from
/// `ft mobile themes`: FrankenTerm's distinct hue themes first, in the order
/// the desktop assigns them, then every built-in color scheme by name.
public struct ThemeCatalog: Sendable {
    public let themes: [WorkspaceDocument.Theme]
    /// The leading themes made to tell machines apart.
    public let distinctCount: Int
    public var distinct: ArraySlice<WorkspaceDocument.Theme> { themes.prefix(distinctCount) }
    private let byName: [String: Int]

    /// The catalog is a bundled resource, not imported input, so it is not
    /// held to the manifest's node cap; every color is still checked.
    public init(data: Data) throws {
        struct Record: Decodable {
            let id: String, name: String, foreground: String, background: String, accent: String
            let cursor_foreground: String, cursor_background: String, cursor_border: String
            let selection_foreground: String, selection_background: String
            let ansi: [String]
            let indexed: [String: String]?
        }
        struct File: Decodable { let format: String, version: Int, distinct: Int, themes: [Record] }
        let file: File
        do { file = try JSONDecoder().decode(File.self, from: data) } catch { throw FTError.malformed("catalog.json") }
        guard file.format == "ftthemes", file.version == 1 else { throw FTError.unsupported("catalog.version") }
        guard (0...file.themes.count).contains(file.distinct) else { throw FTError.malformed("catalog.distinct") }
        let color = WorkspaceDocument.color
        themes = try file.themes.map { record in
            guard record.ansi.count == 16 else { throw FTError.malformed("theme.ansi") }
            var indexed: [UInt8: String] = [:]
            for (key, value) in record.indexed ?? [:] {
                guard let index = UInt8(key), index >= 16, String(index) == key else { throw FTError.malformed("theme.index") }
                indexed[index] = try color(value)
            }
            return WorkspaceDocument.Theme(
                id: try WorkspaceDocument.identifier(record.id), name: try WorkspaceDocument.displayText(record.name, cap: 256),
                foreground: try color(record.foreground), background: try color(record.background),
                accent: try color(record.accent), cursorForeground: try color(record.cursor_foreground),
                cursorBackground: try color(record.cursor_background), cursorBorder: try color(record.cursor_border),
                selectionForeground: try color(record.selection_foreground),
                selectionBackground: try color(record.selection_background),
                ansi: try record.ansi.map(color), indexed: indexed, font: nil)
        }
        var byName: [String: Int] = [:]
        for (index, theme) in themes.enumerated() {
            guard byName.updateValue(index, forKey: theme.name) == nil else { throw FTError.malformed("catalog.duplicate_name") }
        }
        self.byName = byName
        distinctCount = file.distinct
    }

    public func theme(named name: String) -> WorkspaceDocument.Theme? { byName[name].map { themes[$0] } }
}

/// A machine's theme on this phone. `name` names a theme of the workspace or
/// of the catalog; nil keeps the theme the workspace gave the machine.
public struct MachineThemeChoice: Codable, Equatable, Sendable {
    public let name: String?
    /// The phone chose it to tell machines apart; the user did not.
    public let automatic: Bool
    public init(name: String?, automatic: Bool = false) { self.name = name; self.automatic = automatic }
}

/// What each machine of a workspace looks like on this phone.
public struct MachineThemes: Sendable {
    public let document: WorkspaceDocument
    public let catalog: ThemeCatalog
    public let choices: [String: MachineThemeChoice]

    public init(document: WorkspaceDocument, catalog: ThemeCatalog, choices: [String: MachineThemeChoice]) {
        self.document = document; self.catalog = catalog; self.choices = choices
    }

    /// The theme the workspace gave a machine.
    public func workspaceTheme(of machine: WorkspaceDocument.Machine) -> WorkspaceDocument.Theme? {
        document.themes.first { $0.id == machine.themeID }
    }

    /// A theme name resolves to the workspace's theme of that name first,
    /// then the catalog's.
    public func theme(named name: String) -> WorkspaceDocument.Theme? {
        document.themes.first { $0.name == name } ?? catalog.theme(named: name)
    }

    /// The chosen theme; a name neither knows any more falls back.
    public func theme(for machine: WorkspaceDocument.Machine) -> WorkspaceDocument.Theme? {
        if let name = choices[machine.id]?.name, let chosen = theme(named: name) { return chosen }
        return workspaceTheme(of: machine)
    }

    /// Whether a machine has a choice that still applies.
    func hasLiveChoice(_ machine: WorkspaceDocument.Machine) -> Bool {
        guard let choice = choices[machine.id] else { return false }
        return choice.name.map { theme(named: $0) != nil } ?? true
    }

    /// Choices that give no two machines the same look, for a workspace the
    /// user has not themed: every machine that looks like an earlier one gets
    /// the first distinct catalog theme no machine uses, preferring hues at
    /// least 22 degrees from every accent in use. The user's choices are
    /// kept, and so are earlier automatic ones while still needed, so a
    /// machine added later never recolors one that is already distinct; an
    /// automatic choice goes once the workspace's own theme tells the
    /// machine apart (say, after the desktop gave it one).
    public func distinguishingMachines() -> [String: MachineThemeChoice] {
        var result = choices
        var used: [WorkspaceDocument.Theme] = []
        func looksUsed(_ theme: WorkspaceDocument.Theme) -> Bool {
            used.contains { $0.name == theme.name || ($0.background == theme.background && $0.accent == theme.accent) }
        }
        for machine in document.machines {
            let own = workspaceTheme(of: machine)
            if hasLiveChoice(machine), choices[machine.id]?.automatic == true, let own, !looksUsed(own) {
                result[machine.id] = nil
                used.append(own)
                continue
            }
            let current = theme(for: machine)
            if hasLiveChoice(machine) || current.map(looksUsed) != true {
                if let current { used.append(current) }
                continue
            }
            // Like the desktop: the first free distinct theme 22 degrees clear
            // of every accent, else the free one farthest from them; past the
            // distinct themes, the farthest free scheme.
            let nearest = { (theme: WorkspaceDocument.Theme) in
                used.map { Self.hueDistance($0.accent, theme.accent) }.min() ?? 180
            }
            let free = catalog.distinct.filter { !looksUsed($0) }
            guard let pick = free.first(where: { nearest($0) >= 22 })
                ?? free.max(by: { nearest($0) < nearest($1) })
                ?? catalog.themes.dropFirst(catalog.distinctCount).filter({ !looksUsed($0) })
                    .max(by: { nearest($0) < nearest($1) }) else { continue }
            result[machine.id] = MachineThemeChoice(name: pick.name, automatic: true)
            used.append(pick)
        }
        return result
    }

    /// Degrees between two colors' hues; a gray has no hue to clash with.
    static func hueDistance(_ a: String, _ b: String) -> Double {
        guard let first = hue(a), let second = hue(b) else { return 180 }
        let distance = abs(first - second).truncatingRemainder(dividingBy: 360)
        return min(distance, 360 - distance)
    }

    static func hue(_ hex: String) -> Double? {
        guard let value = UInt32(hex.dropFirst().prefix(6), radix: 16) else { return nil }
        let r = Double(value >> 16 & 0xFF) / 255, g = Double(value >> 8 & 0xFF) / 255, b = Double(value & 0xFF) / 255
        let high = max(r, g, b), low = min(r, g, b), chroma = high - low
        guard chroma > 0.08 else { return nil }
        let sector: Double
        switch high {
        case r: sector = ((g - b) / chroma).truncatingRemainder(dividingBy: 6)
        case g: sector = (b - r) / chroma + 2
        default: sector = (r - g) / chroma + 4
        }
        let degrees = sector * 60
        return degrees < 0 ? degrees + 360 : degrees
    }
}
