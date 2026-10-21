import Foundation
import XCTest
@testable import FrankenTermKit

/// Both files are written by the desktop (`ft mobile themes`, `ft mobile
/// export`); a Rust test keeps them equal to what it writes today.
final class MachineThemesTests: XCTestCase {
    private static let repository = (0..<6).reduce(URL(fileURLWithPath: #filePath)) { url, _ in url.deletingLastPathComponent() }

    private func catalog() throws -> ThemeCatalog {
        try ThemeCatalog(data: Data(contentsOf: Self.repository.appendingPathComponent("apps/ios/FrankenTerm/ThemeCatalog.json")))
    }

    private func export(change: (inout [[String: Any]]) -> Void = { _ in }) throws -> WorkspaceDocument {
        let data = try Data(contentsOf: Self.repository.appendingPathComponent("fixtures/ios-airgap/machines-export.ftworkspace"))
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var themes = try XCTUnwrap(workspace["themes"] as? [[String: Any]])
        change(&themes)
        workspace["themes"] = themes; root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
    }

    /// Every machine wears a copy of the first machine's theme under its own id.
    private func lookalikes() throws -> WorkspaceDocument {
        try export { themes in
            let first = themes[0]
            themes = themes.map { theme in var copy = first; copy["id"] = theme["id"]; return copy }
        }
    }

    private func machine(_ document: WorkspaceDocument, _ id: String) throws -> WorkspaceDocument.Machine {
        try XCTUnwrap(document.machines.first { $0.id == id })
    }

    func testTheBundledCatalogOffersTheDistinctThemesFirstThenEveryScheme() throws {
        let catalog = try catalog()
        XCTAssertEqual(catalog.distinctCount, 12)
        XCTAssertEqual(catalog.themes.first?.name, "FrankenTerm Azure")
        XCTAssertTrue(catalog.distinct.allSatisfy { $0.name.hasPrefix("FrankenTerm ") })
        XCTAssertGreaterThan(catalog.themes.count, 1000)
        let dracula = try XCTUnwrap(catalog.theme(named: "Dracula"))
        XCTAssertEqual(dracula.terminalBasePalette.count, 1052)
        XCTAssertNil(catalog.theme(named: "dracula"), "names are exact")
        XCTAssertEqual(Set(catalog.distinct.map(\.background)).count, catalog.distinctCount)
    }

    /// More lookalike machines than distinct themes: each still gets its own
    /// theme, the first ones 22 degrees apart, the rest as far as possible.
    func testAFleetLargerThanTheDistinctThemesNeverRepeatsATheme() throws {
        let catalog = try catalog()
        let count = catalog.distinctCount + 3
        let data = try Data(contentsOf: Self.repository.appendingPathComponent("fixtures/ios-airgap/machines-export.ftworkspace"))
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        let theme = try XCTUnwrap((workspace["themes"] as? [[String: Any]])?.first)
        let machine = try XCTUnwrap((workspace["machines"] as? [[String: Any]])?.first)
        var machines: [[String: Any]] = [], themes: [[String: Any]] = []
        for index in 0..<count {
            var copy = machine; copy["id"] = "m.\(index)"; copy["theme_ref"] = "t.\(index)"
            var look = theme; look["id"] = "t.\(index)"
            machines.append(copy); themes.append(look)
        }
        var groups = try XCTUnwrap(workspace["groups"] as? [[String: Any]])
        groups[0]["machine_order"] = machines.map { $0["id"]! }
        workspace["machines"] = machines; workspace["themes"] = themes; workspace["groups"] = groups
        var preferences = try XCTUnwrap(workspace["preferences"] as? [String: Any])
        preferences["initial_machine_ref"] = "m.0"; workspace["preferences"] = preferences
        root["workspace"] = workspace
        let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        let choices = MachineThemes(document: document, catalog: catalog, choices: [:]).distinguishingMachines()
        XCTAssertEqual(choices.count, count - 1)
        let resolved = MachineThemes(document: document, catalog: catalog, choices: choices)
        let names = document.machines.compactMap { resolved.theme(for: $0)?.name }
        XCTAssertEqual(Set(names).count, count, "\(names)")
        let accents = document.machines.prefix(4).compactMap { resolved.theme(for: $0)?.accent }
        for (index, accent) in accents.enumerated() {
            for other in accents.prefix(index) { XCTAssertGreaterThanOrEqual(MachineThemes.hueDistance(accent, other), 22) }
        }
    }

    func testACatalogWithABadColorOrDuplicateNameIsRefused() throws {
        func catalog(_ themes: [[String: Any]]) throws -> ThemeCatalog {
            try ThemeCatalog(data: JSONSerialization.data(withJSONObject: ["format": "ftthemes", "version": 1, "distinct": 0, "themes": themes]))
        }
        let theme: [String: Any] = ["id": "catalog.0", "name": "A", "foreground": "#FFFFFF", "background": "#000000",
            "accent": "#FF0000", "cursor_foreground": "#000000", "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF",
            "selection_foreground": "#FFFFFF", "selection_background": "#333333", "ansi": Array(repeating: "#123456", count: 16)]
        XCTAssertEqual(try catalog([theme]).themes.count, 1)
        var bad = theme; bad["accent"] = "red"
        XCTAssertThrowsError(try catalog([bad]))
        var short = theme; short["ansi"] = ["#123456"]
        XCTAssertThrowsError(try catalog([short]))
        var twin = theme; twin["id"] = "catalog.1"
        XCTAssertThrowsError(try catalog([theme, twin]))
    }

    func testTheRustExportPassesTheStrictParserWithItsThemes() throws {
        let document = try export()
        XCTAssertEqual(document.exporterID, "frankenterm.machines")
        XCTAssertEqual(document.machines.map(\.id), ["m.trj", "m.css", "m.dark"])
        let themes = MachineThemes(document: document, catalog: try catalog(), choices: [:])
        let trj = try XCTUnwrap(themes.theme(for: try machine(document, "m.trj")))
        XCTAssertEqual(trj.background, "#0A0E14")
        // The desktop already made each machine distinct: nothing to change.
        XCTAssertEqual(themes.distinguishingMachines(), [:])
    }

    func testMachinesThatLookAlikeGetDistinctCatalogThemesOnce() throws {
        let document = try lookalikes(), catalog = try catalog()
        let choices = MachineThemes(document: document, catalog: catalog, choices: [:]).distinguishingMachines()
        XCTAssertNil(choices["m.trj"], "the first machine keeps its theme")
        for id in ["m.css", "m.dark"] {
            let choice = try XCTUnwrap(choices[id])
            XCTAssertTrue(choice.automatic)
            XCTAssertTrue(catalog.distinct.contains { $0.name == choice.name })
        }
        let themes = MachineThemes(document: document, catalog: catalog, choices: choices)
        let resolved = try document.machines.map { try XCTUnwrap(themes.theme(for: $0)) }
        XCTAssertEqual(Set(resolved.map(\.name)).count, 3)
        XCTAssertEqual(Set(resolved.map(\.background)).count, 3)
        for (index, theme) in resolved.enumerated() {
            for other in resolved.prefix(index) {
                XCTAssertGreaterThanOrEqual(MachineThemes.hueDistance(theme.accent, other.accent), 22)
            }
        }
        XCTAssertEqual(themes.distinguishingMachines(), choices, "a second pass changes nothing")
    }

    func testTheUsersChoicesStandAndOnlyStaleOrMissingOnesAreFilledIn() throws {
        let document = try lookalikes(), catalog = try catalog()
        let picked = MachineThemeChoice(name: "Dracula", catalog: true)
        var choices = MachineThemes(document: document, catalog: catalog,
                                    choices: ["m.css": picked]).distinguishingMachines()
        XCTAssertEqual(choices["m.css"], picked)
        XCTAssertEqual(choices["m.dark"]?.automatic, true)
        XCTAssertNotEqual(choices["m.dark"]?.name, "Dracula")

        // Keeping the workspace's own theme is a choice too.
        let own = MachineThemeChoice(name: nil)
        choices = MachineThemes(document: document, catalog: catalog, choices: ["m.css": own, "m.dark": own]).distinguishingMachines()
        XCTAssertEqual(choices, ["m.css": own, "m.dark": own])

        // A name no theme has any more falls back, and is replaced when that
        // fallback looks like another machine.
        let stale = MachineThemes(document: document, catalog: catalog, choices: ["m.css": .init(name: "Gone")])
        XCTAssertEqual(stale.theme(for: try machine(document, "m.css"))?.background, "#0A0E14")
        XCTAssertEqual(stale.distinguishingMachines()["m.css"]?.automatic, true)
    }

    func testAMachineAddedLaterDoesNotRecolorTheOthers() throws {
        let document = try lookalikes(), catalog = try catalog()
        // The first free theme for a second machine, when nothing was chosen.
        let fresh = MachineThemes(document: document, catalog: catalog, choices: [:]).distinguishingMachines()
        let first = try XCTUnwrap(fresh["m.css"])
        // css already wears an earlier automatic pick; dark is new.
        let later = MachineThemes(document: document, catalog: catalog,
                                  choices: ["m.css": .init(name: catalog.distinct.last?.name, catalog: true, automatic: true)]).distinguishingMachines()
        XCTAssertEqual(later["m.css"]?.name, catalog.distinct.last?.name, "an earlier automatic choice is kept")
        XCTAssertEqual(later["m.dark"]?.name, first.name, "the newcomer takes the first free theme")
    }

    func testAutomaticChoicesGoOnceTheWorkspaceTellsMachinesApart() throws {
        let catalog = try catalog()
        let automatic = MachineThemes(document: try lookalikes(), catalog: catalog, choices: [:]).distinguishingMachines()
        XCTAssertEqual(automatic.count, 2)
        // The desktop now exports a distinct theme for every machine.
        let picked = MachineThemeChoice(name: "Dracula", catalog: true)
        var kept = automatic; kept["m.trj"] = picked
        let choices = MachineThemes(document: try export(), catalog: catalog, choices: kept).distinguishingMachines()
        XCTAssertEqual(choices, ["m.trj": picked], "automatic picks give way to the workspace; the user's pick stays")
    }

    /// A desktop machine can wear a modified "Dracula": a pick names its source.
    func testAChoiceResolvesInTheSourceItNamesWhenNamesCollide() throws {
        let catalog = try catalog()
        let document = try export { themes in themes[1]["name"] = "Dracula"; themes[1]["background"] = "#123456" }
        let css = try machine(document, "m.css"), trj = try machine(document, "m.trj")
        let fromCatalog = MachineThemes(document: document, catalog: catalog,
                                        choices: ["m.trj": .init(name: "Dracula", catalog: true)])
        XCTAssertEqual(fromCatalog.theme(for: trj)?.background, catalog.theme(named: "Dracula")?.background)
        XCTAssertNotEqual(fromCatalog.theme(for: trj)?.background, "#123456")
        let fromWorkspace = MachineThemes(document: document, catalog: catalog,
                                          choices: ["m.trj": .init(name: "Dracula")])
        XCTAssertEqual(fromWorkspace.theme(for: trj)?.background, "#123456")
        XCTAssertEqual(fromWorkspace.theme(for: trj)?.id, fromWorkspace.workspaceTheme(of: css)?.id)
        let saved = try JSONEncoder().encode(MachineThemeChoice(name: "Dracula", catalog: true))
        XCTAssertEqual(try JSONDecoder().decode(MachineThemeChoice.self, from: saved), MachineThemeChoice(name: "Dracula", catalog: true))
    }

    func testHueDistanceWrapsAndIgnoresGrays() {
        XCTAssertEqual(MachineThemes.hueDistance("#FF0000", "#0000FF"), 120, accuracy: 0.5)
        XCTAssertEqual(MachineThemes.hueDistance("#FF0010", "#FF1000"), 7.5, accuracy: 0.5)
        XCTAssertEqual(MachineThemes.hueDistance("#808080", "#FF0000"), 180)
    }
}
