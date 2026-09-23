//! ft-og9pc — the Robot Mode ↔ MCP surface matrix covers both surfaces exactly.
//!
//! `docs/robot-contracts/mcp-robot-surface-matrix.md` replaces the old blanket
//! "MCP mirrors Robot Mode" claim with a scoped per-family table. This guard
//! fails when a `RobotCommands` family or a registered MCP tool (the golden
//! manifest, itself checked against the live server) is added, removed, or
//! renamed without the matrix following, and when a row's parity level
//! contradicts its own columns.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const MATRIX_PATH: &str = "docs/robot-contracts/mcp-robot-surface-matrix.md";
const PARITY_LEVELS: &[&str] = &["mirrored", "partial", "robot-only", "mcp-only"];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(workspace_root().join(relative))
        .unwrap_or_else(|err| panic!("read {relative}: {err}"))
}

/// Top-level variant names of `enum RobotCommands`.
fn robot_families(main_rs: &str) -> BTreeSet<String> {
    let mut families = BTreeSet::new();
    let mut inside = false;
    for line in main_rs.lines() {
        if !inside {
            inside = line.trim_start_matches("pub ").starts_with("enum RobotCommands {");
            continue;
        }
        if line.starts_with('}') {
            break;
        }
        let Some(rest) = line.strip_prefix("    ") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        families.insert(name);
    }
    families
}

fn mcp_tools(manifest: &str) -> BTreeSet<String> {
    let manifest: serde_json::Value = serde_json::from_str(manifest).expect("manifest parses");
    manifest["tools"]
        .as_array()
        .expect("manifest tools array")
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name").to_string())
        .collect()
}

/// Backticked items in one table cell; `—` means none.
fn cell_items(cell: &str) -> Vec<String> {
    cell.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

struct Row {
    family: Option<String>,
    tools: Vec<String>,
    parity: String,
}

fn matrix_rows(matrix: &str) -> Vec<Row> {
    matrix
        .lines()
        .filter(|line| line.starts_with("| `") || line.starts_with("| — |"))
        .map(|line| {
            let cells: Vec<&str> = line.trim_matches('|').split(" | ").map(str::trim).collect();
            assert_eq!(cells.len(), 5, "matrix row must have 5 cells: {line}");
            Row {
                family: cell_items(cells[0]).into_iter().next(),
                tools: cell_items(cells[2]),
                parity: cells[3].to_string(),
            }
        })
        .collect()
}

#[test]
fn matrix_covers_every_robot_family_and_mcp_tool_exactly_once() {
    let families = robot_families(&read("crates/frankenterm/src/main.rs"));
    let tools = mcp_tools(&read(
        "crates/frankenterm-core/tests/fixtures/mcp_manifest.json",
    ));
    assert!(families.len() > 40, "parsed only {} families", families.len());
    assert!(tools.len() > 30, "parsed only {} tools", tools.len());

    let rows = matrix_rows(&read(MATRIX_PATH));
    let mut family_rows: BTreeMap<String, usize> = BTreeMap::new();
    let mut tool_rows: BTreeMap<String, usize> = BTreeMap::new();
    for row in &rows {
        if let Some(family) = &row.family {
            *family_rows.entry(family.clone()).or_default() += 1;
        }
        for tool in &row.tools {
            *tool_rows.entry(tool.clone()).or_default() += 1;
        }
    }

    let duplicated: Vec<_> = family_rows
        .iter()
        .chain(tool_rows.iter())
        .filter(|(_, count)| **count > 1)
        .map(|(name, _)| name.as_str())
        .collect();
    assert!(duplicated.is_empty(), "listed more than once: {duplicated:?}");

    let listed_families: BTreeSet<String> = family_rows.into_keys().collect();
    let listed_tools: BTreeSet<String> = tool_rows.into_keys().collect();
    assert_eq!(
        families.difference(&listed_families).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "Robot families missing from {MATRIX_PATH}"
    );
    assert_eq!(
        listed_families.difference(&families).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "{MATRIX_PATH} lists Robot families that no longer exist"
    );
    assert_eq!(
        tools.difference(&listed_tools).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "MCP tools missing from {MATRIX_PATH}"
    );
    assert_eq!(
        listed_tools.difference(&tools).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "{MATRIX_PATH} lists MCP tools that are not registered"
    );
}

#[test]
fn matrix_parity_levels_agree_with_their_columns() {
    for row in matrix_rows(&read(MATRIX_PATH)) {
        let label = row
            .family
            .clone()
            .or_else(|| row.tools.first().cloned())
            .unwrap_or_default();
        assert!(
            PARITY_LEVELS.contains(&row.parity.as_str()),
            "{label}: unknown parity level {:?}",
            row.parity
        );
        let (has_family, has_tools) = (row.family.is_some(), !row.tools.is_empty());
        let consistent = match row.parity.as_str() {
            "robot-only" => has_family && !has_tools,
            "mcp-only" => !has_family && has_tools,
            _ => has_family && has_tools,
        };
        assert!(
            consistent,
            "{label}: parity {} contradicts its columns",
            row.parity
        );
    }
}

#[test]
fn robot_family_parser_reads_top_level_variants_only() {
    let source = "#[derive(Subcommand)]\nenum RobotCommands {\n    /// doc\n    State {\n        /// nested\n        Pane(u64),\n    },\n    #[command(name = \"x\")]\n    GetText(Args),\n    Help,\n}\nenum Other {\n    Nope,\n}\n";
    assert_eq!(
        robot_families(source).into_iter().collect::<Vec<_>>(),
        vec!["GetText", "Help", "State"]
    );
}
