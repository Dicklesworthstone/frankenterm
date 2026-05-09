//! Conformance gate for the outward-facing Robot ApiSurface coverage matrix.
//!
//! `robot_api_contracts::ApiSurface::ALL` is the code-side list of machine
//! surfaces. This test keeps the docs/schema/golden/proof ledger in
//! `docs/robot-contracts/api-surface-coverage.md` synchronized with that list.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::api_schema::SchemaRegistry;
use frankenterm_core::robot_api_contracts::ApiSurface;

const MATRIX_REL_PATH: &str = "docs/robot-contracts/api-surface-coverage.md";
const FOLLOW_UP_BEAD: &str = "ft-luisq";

#[derive(Debug, Clone)]
struct CoverageRow {
    surface: String,
    category: String,
    schema_artifact: String,
    docs_artifact: String,
    golden_artifact: String,
    proof_lane: String,
    status: String,
}

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn load_matrix() -> String {
    let path = workspace_root().join(MATRIX_REL_PATH);
    fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()))
}

fn markdown_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(str::trim)
        .map(ToOwned::to_owned)
        .collect()
}

fn strip_code_ticks(value: &str) -> String {
    value
        .strip_prefix('`')
        .and_then(|rest| rest.strip_suffix('`'))
        .unwrap_or(value)
        .to_string()
}

fn parse_rows(markdown: &str) -> Result<Vec<CoverageRow>, Vec<String>> {
    let mut in_matrix = false;
    let mut rows = Vec::new();
    let mut errors = Vec::new();

    for (line_index, line) in markdown.lines().enumerate() {
        if line.trim() == "## Matrix" {
            in_matrix = true;
            continue;
        }

        if !in_matrix || !line.trim_start().starts_with('|') {
            continue;
        }

        let cells = markdown_cells(line);
        if cells.first().is_some_and(|cell| cell.as_str() == "Surface")
            || cells.first().is_some_and(|cell| cell.starts_with("---"))
        {
            continue;
        }

        if cells.len() != 9 {
            errors.push(format!(
                "line {} has {} columns, expected 9: {line}",
                line_index + 1,
                cells.len()
            ));
            continue;
        }

        rows.push(CoverageRow {
            surface: strip_code_ticks(&cells[0]),
            category: strip_code_ticks(&cells[1]),
            schema_artifact: cells[4].clone(),
            docs_artifact: cells[5].clone(),
            golden_artifact: cells[6].clone(),
            proof_lane: strip_code_ticks(&cells[7]),
            status: cells[8].clone(),
        });
    }

    if rows.is_empty() {
        errors.push("coverage matrix has no data rows".to_string());
    }

    if errors.is_empty() {
        Ok(rows)
    } else {
        Err(errors)
    }
}

fn code_spans(value: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut rest = value;

    while let Some(start) = rest.find('`') {
        let after_start = &rest[start + 1..];
        let Some(end) = after_start.find('`') else {
            break;
        };
        spans.push(after_start[..end].to_string());
        rest = &after_start[end + 1..];
    }

    spans
}

fn artifact_paths(value: &str) -> Vec<String> {
    code_spans(value)
        .into_iter()
        .filter_map(|span| {
            let path = span
                .split_once("::")
                .map_or(span.as_str(), |(path, _)| path);
            if path.contains('/') {
                Some(path.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn is_deferred(value: &str) -> bool {
    value.trim_start().starts_with("DEFERRED(")
}

fn validate_artifact_cell(
    row: &CoverageRow,
    cell_name: &str,
    value: &str,
    allow_deferred: bool,
    root: &Path,
    errors: &mut Vec<String>,
) {
    let normalized = value.trim();
    let lowered = normalized.to_ascii_lowercase();
    if lowered == "n/a" || lowered == "none" || lowered == "tbd" {
        errors.push(format!(
            "{} {cell_name} uses undocumented placeholder `{normalized}`",
            row.surface
        ));
        return;
    }

    if is_deferred(normalized) {
        if !allow_deferred {
            errors.push(format!(
                "{} {cell_name} is deferred, but this column requires a real artifact",
                row.surface
            ));
        }
        let expected = format!("DEFERRED({FOLLOW_UP_BEAD}): ");
        if !normalized.starts_with(&expected) {
            errors.push(format!(
                "{} {cell_name} deferred marker must start with `{expected}`",
                row.surface
            ));
        }
        if normalized.len() <= expected.len() + 10 {
            errors.push(format!(
                "{} {cell_name} deferred marker needs a concrete rationale",
                row.surface
            ));
        }
        return;
    }

    let paths = artifact_paths(normalized);
    if paths.is_empty() {
        errors.push(format!(
            "{} {cell_name} must contain at least one backticked repo path or an explicit DEFERRED({FOLLOW_UP_BEAD}) marker",
            row.surface
        ));
        return;
    }

    for path in paths {
        let abs = root.join(&path);
        if !abs.exists() {
            errors.push(format!(
                "{} {cell_name} path does not exist: {path}",
                row.surface
            ));
        }
    }
}

fn validate_surface_set(rows: &[CoverageRow]) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    let mut by_surface: BTreeMap<&str, &CoverageRow> = BTreeMap::new();

    for row in rows {
        if by_surface.insert(row.surface.as_str(), row).is_some() {
            errors.push(format!("duplicate coverage row for {}", row.surface));
        }
    }

    let expected: BTreeSet<&str> = ApiSurface::ALL
        .iter()
        .map(ApiSurface::command_name)
        .collect();
    let actual: BTreeSet<&str> = rows.iter().map(|row| row.surface.as_str()).collect();

    for missing in expected.difference(&actual) {
        errors.push(format!("missing ApiSurface row: {missing}"));
    }
    for stale in actual.difference(&expected) {
        errors.push(format!("stale matrix row not in ApiSurface::ALL: {stale}"));
    }

    for surface in ApiSurface::ALL {
        if let Some(row) = by_surface.get(surface.command_name()) {
            let expected_category = surface.category();
            if row.category != expected_category {
                errors.push(format!(
                    "{} category mismatch: matrix has `{}`, code has `{expected_category}`",
                    row.surface, row.category
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_artifact_references(rows: &[CoverageRow]) -> Result<(), Vec<String>> {
    let root = workspace_root();
    let schema_provenance =
        fs::read_to_string(root.join("docs/json-schema/PROVENANCE.md")).unwrap_or_default();
    let registered_schemas: BTreeSet<String> = SchemaRegistry::canonical()
        .schema_files()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();
    let mut errors = Vec::new();

    for row in rows {
        validate_artifact_cell(
            row,
            "schema artifact",
            &row.schema_artifact,
            true,
            &root,
            &mut errors,
        );
        validate_artifact_cell(
            row,
            "docs artifact",
            &row.docs_artifact,
            false,
            &root,
            &mut errors,
        );
        validate_artifact_cell(
            row,
            "golden artifact",
            &row.golden_artifact,
            true,
            &root,
            &mut errors,
        );

        let has_deferred = is_deferred(&row.schema_artifact) || is_deferred(&row.golden_artifact);
        if has_deferred && !row.status.contains(FOLLOW_UP_BEAD) {
            errors.push(format!(
                "{} has deferred artifacts but status does not name {FOLLOW_UP_BEAD}",
                row.surface
            ));
        }
        if !has_deferred && row.status != "COVERED" {
            errors.push(format!(
                "{} has direct artifacts but status is `{}` instead of COVERED",
                row.surface, row.status
            ));
        }

        for schema_path in artifact_paths(&row.schema_artifact) {
            let Some(schema_file) = schema_path.strip_prefix("docs/json-schema/") else {
                continue;
            };
            if schema_file == "wa-robot-envelope.json" || schema_file == "wa-mcp-envelope.json" {
                continue;
            }
            if !registered_schemas.contains(schema_file) {
                errors.push(format!(
                    "{} schema {schema_file} is not registered in SchemaRegistry::canonical()",
                    row.surface
                ));
            }
            let provenance_token = format!("| `{schema_file}` |");
            if !schema_provenance.contains(&provenance_token) {
                errors.push(format!(
                    "{} schema {schema_file} is not documented in docs/json-schema/PROVENANCE.md",
                    row.surface
                ));
            }
        }

        if !row.proof_lane.starts_with("rch exec -- env ") {
            errors.push(format!(
                "{} proof lane must run through rch: {}",
                row.surface, row.proof_lane
            ));
        }
        if !row.proof_lane.contains("cargo test -p frankenterm-core") {
            errors.push(format!(
                "{} proof lane must identify the frankenterm-core cargo test target",
                row.surface
            ));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[test]
fn api_surface_coverage_matrix_accounts_for_every_surface() {
    let matrix = load_matrix();
    let rows = parse_rows(&matrix)
        .unwrap_or_else(|errors| panic!("failed to parse coverage matrix:\n{}", errors.join("\n")));

    validate_surface_set(&rows)
        .unwrap_or_else(|errors| panic!("ApiSurface coverage drift:\n{}", errors.join("\n")));

    let categories: BTreeSet<&str> = rows.iter().map(|row| row.category.as_str()).collect();
    let expected_categories: BTreeSet<&str> =
        ApiSurface::ALL.iter().map(ApiSurface::category).collect();
    assert_eq!(
        categories, expected_categories,
        "matrix must report coverage for every ApiSurface category"
    );
}

#[test]
fn api_surface_coverage_artifacts_exist_or_name_deferred_bead() {
    let matrix = load_matrix();
    let rows = parse_rows(&matrix)
        .unwrap_or_else(|errors| panic!("failed to parse coverage matrix:\n{}", errors.join("\n")));

    validate_artifact_references(&rows).unwrap_or_else(|errors| {
        panic!(
            "ApiSurface artifact reference drift:\n{}",
            errors.join("\n")
        )
    });
}

#[test]
fn matrix_parser_rejects_omitted_surface() {
    let markdown = r#"
## Matrix

| Surface | Category | Robot CLI | MCP surface | Schema artifact | Docs artifact | Golden or matrix artifact | Proof lane | Status |
|---|---|---|---|---|---|---|---|---|
| `get-text` | `pane` | `ft robot get-text <pane_id>` | `wa.get_text` | `docs/json-schema/wa-robot-get-text.json` | `docs/cli-reference.md` | `crates/frankenterm-core/tests/golden_robot_envelope/wa_get_text.json` | `rch exec -- env CARGO_TARGET_DIR=/tmp/ft-b7ysg-api-surface cargo test -p frankenterm-core --test conformance_robot_api_surface_coverage -- --nocapture` | COVERED |
"#;
    let rows = parse_rows(markdown).expect("fixture matrix should parse");
    let errors = validate_surface_set(&rows).expect_err("omitted surfaces must fail");

    assert!(
        errors
            .iter()
            .any(|error| error.contains("missing ApiSurface row: state")),
        "expected missing-surface diagnostic for state, got:\n{}",
        errors.join("\n")
    );
}
