//! CI-gating coverage matrix for MCP spec MUST/SHOULD clauses
//! (bead ft-zaqi8).
//!
//! Parses `docs/mcp-api-spec.md` and its coverage matrix, requires every
//! explicit normative clause to carry exactly one stable ID, and asserts that
//! every clause marked `TESTED` has an annotation in the crate's unit or
//! integration test corpus. Clauses marked `DEFERRED` are tracked in the
//! matrix but exempt from the test-annotation gate.
//!
//! Why this exists: the conformance skill mandates a coverage
//! accounting matrix for every spec MUST/SHOULD. Without a CI gate,
//! a new MUST line in `docs/mcp-api-spec.md` can land without test
//! coverage — silently breaking the contract for downstream MCP
//! clients (TypeScript SDKs, AI agents).
//!
//! Add a clause: edit `docs/mcp-api-spec-coverage.md`, add a row
//! with the next free ID (Status=TESTED), then add a `MCP-V1-NNN`
//! annotation to the test that enforces it. Re-run this test —
//! green means coverage is in place.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is .../crates/frankenterm-core ; the matrix
    // doc lives at the workspace root's docs/ directory.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn coverage_matrix_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("mcp-api-spec-coverage.md")
}

fn spec_path() -> PathBuf {
    workspace_root().join("docs").join("mcp-api-spec.md")
}

fn test_source_roots() -> [PathBuf; 2] {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    [crate_root.join("src"), crate_root.join("tests")]
}

/// Extracted clause from the coverage matrix.
#[derive(Debug, Clone)]
struct Clause {
    id: String,
    level: String,
    status: String,
}

/// Parse the coverage matrix table rows. Each row begins with `|
/// MCP-V1-NNN |` (or backtick-wrapped). Status is the 4th column
/// (`TESTED` / `DEFERRED`).
fn parse_clauses(matrix_md: &str) -> Vec<Clause> {
    let mut out = Vec::new();
    for line in matrix_md.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            continue;
        }
        // Split into cells, drop leading/trailing empty entries.
        let cells: Vec<&str> = trimmed
            .split('|')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if cells.len() < 4 {
            continue;
        }
        // Column 0 = ID (wrapped in backticks). Skip the table header
        // (`ID`) and the separator row (`---`).
        let id_cell = cells[0].trim_matches('`');
        if !id_cell.starts_with("MCP-V1-") {
            continue;
        }
        // Column 3 = Status (after Section, Level).
        let status = cells[3].to_string();
        out.push(Clause {
            id: id_cell.to_string(),
            level: cells[2].to_string(),
            status,
        });
    }
    out
}

/// Walk the crate's unit and integration test sources and collect every
/// `MCP-V1-NNN` mention. Looks at both module-level docs (`//!`) and inline
/// comments (`//`) so annotations can live beside the enforcing test.
fn collect_ids(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("MCP-V1-") {
        let after = &rest[start + 7..];
        let id_chars: String = after
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        if id_chars.is_empty() {
            rest = after;
            continue;
        }
        found.push(format!("MCP-V1-{id_chars}"));
        rest = &after[id_chars.len()..];
    }
    found
}

#[derive(Debug, Default)]
struct TestAnnotations {
    ids: Vec<String>,
    orphaned: Vec<String>,
}

fn collect_annotations_in(
    path: &std::path::Path,
    annotations: &mut TestAnnotations,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_annotations_in(&path, annotations)?;
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let body = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let lines = body.lines().collect::<Vec<_>>();
        for (index, line) in lines.iter().enumerate() {
            let ids = collect_ids(line);
            if ids.is_empty() {
                continue;
            }
            let window_start = index.saturating_sub(3);
            let window_end = (index + 4).min(lines.len());
            if lines[window_start..window_end]
                .iter()
                .any(|nearby| nearby.trim() == "#[test]")
            {
                annotations.ids.extend(ids);
            } else {
                annotations.orphaned.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    index + 1,
                    line.trim()
                ));
            }
        }
    }
    Ok(())
}

fn collect_annotations(source_roots: &[PathBuf]) -> std::io::Result<TestAnnotations> {
    let mut annotations = TestAnnotations::default();
    for root in source_roots {
        collect_annotations_in(root, &mut annotations)?;
    }
    Ok(annotations)
}

fn is_normative_spec_line(line: &str) -> bool {
    line.split(|character: char| !character.is_ascii_alphabetic())
        .any(|word| matches!(word, "MUST" | "SHOULD" | "REQUIRED"))
}

#[test]
fn matrix_file_exists_and_has_clauses() {
    let path = coverage_matrix_path();
    let body = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "missing coverage matrix at {}: {err}. ft-zaqi8 requires this file.",
            path.display()
        )
    });
    let clauses = parse_clauses(&body);
    assert!(
        !clauses.is_empty(),
        "coverage matrix at {} has no clauses; expected at least one MCP-V1-NNN row",
        path.display()
    );
}

#[test]
fn every_normative_spec_clause_has_one_matching_matrix_id() {
    let spec = std::fs::read_to_string(spec_path()).expect("read MCP API spec");
    let matrix = std::fs::read_to_string(coverage_matrix_path()).expect("read coverage matrix");
    let clauses = parse_clauses(&matrix);
    let matrix_normative = clauses
        .iter()
        .filter(|clause| matches!(clause.level.as_str(), "MUST" | "SHOULD" | "REQUIRED"))
        .map(|clause| clause.id.clone())
        .collect::<std::collections::BTreeSet<_>>();

    let mut spec_normative = std::collections::BTreeSet::new();
    let mut malformed = Vec::new();
    for (index, line) in spec.lines().enumerate() {
        if !is_normative_spec_line(line) {
            continue;
        }
        let ids = collect_ids(line);
        if ids.len() != 1 {
            malformed.push(format!("line {} has {} IDs: {line}", index + 1, ids.len()));
            continue;
        }
        if !spec_normative.insert(ids[0].clone()) {
            malformed.push(format!("line {} repeats ID {}", index + 1, ids[0]));
        }
    }

    assert!(
        malformed.is_empty(),
        "every explicit MCP MUST/SHOULD/REQUIRED line must carry exactly one unique MCP-V1-NNN ID: {malformed:?}"
    );
    assert_eq!(
        spec_normative, matrix_normative,
        "normative IDs in the MCP spec and coverage matrix have drifted"
    );
}

#[test]
fn every_tested_clause_has_at_least_one_annotation() {
    let body = std::fs::read_to_string(coverage_matrix_path())
        .expect("read coverage matrix (matrix_file_exists_and_has_clauses runs first)");
    let clauses = parse_clauses(&body);

    let annotations = collect_annotations(&test_source_roots())
        .expect("walk src/ and tests/ for MCP-V1-NNN annotations");
    assert!(
        annotations.orphaned.is_empty(),
        "MCP clause annotations must be adjacent to an actual #[test] attribute: {:?}",
        annotations.orphaned
    );

    let mut missing = Vec::new();
    let mut counts = Vec::new();
    for clause in &clauses {
        if clause.status != "TESTED" {
            continue;
        }
        let n = annotations
            .ids
            .iter()
            .filter(|annotation| annotation == &&clause.id)
            .count();
        counts.push((clause.id.clone(), n));
        if n == 0 {
            missing.push(clause.id.clone());
        }
    }

    eprintln!(
        "{}",
        serde_json::json!({
            "phase": "report",
            "suite": "mcp_coverage",
            "clauses": counts.iter().map(|(id, n)| serde_json::json!({"id": id, "annotations": n})).collect::<Vec<_>>(),
            "missing": missing.clone(),
        })
    );

    assert!(
        missing.is_empty(),
        "clauses in docs/mcp-api-spec-coverage.md marked TESTED but with NO `MCP-V1-NNN` \
         annotation in the test corpus: {missing:?}\n\n\
         Add a `// MCP-V1-NNN` comment to a test that enforces the clause, \
         OR change the row's Status to DEFERRED with a follow-up note.",
    );
}

#[test]
fn every_annotation_corresponds_to_a_matrix_clause() {
    let body = std::fs::read_to_string(coverage_matrix_path()).expect("read coverage matrix");
    let clauses = parse_clauses(&body);
    let known_ids: std::collections::HashSet<String> =
        clauses.iter().map(|c| c.id.clone()).collect();

    let annotations = collect_annotations(&test_source_roots())
        .expect("walk src/ and tests/ for MCP-V1-NNN annotations");

    let unknown: Vec<String> = annotations
        .ids
        .iter()
        .filter(|a| !known_ids.contains(a.as_str()))
        .cloned()
        .collect();

    let mut unknown_dedup = unknown.clone();
    unknown_dedup.sort();
    unknown_dedup.dedup();

    assert!(
        unknown_dedup.is_empty(),
        "test annotations reference clause IDs that are not in \
         docs/mcp-api-spec-coverage.md: {unknown_dedup:?}\n\n\
         Either add a row to the matrix for each, or fix the typo \
         in the annotation comment.",
    );
}
