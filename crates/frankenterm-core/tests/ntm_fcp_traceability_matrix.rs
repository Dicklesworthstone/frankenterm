//! ft-3681t.1.5.1 — traceability matrix consistency checks.
//!
//! These tests validate that the machine-checkable NTM/FCP convergence matrix:
//! 1. has required schema fields,
//! 2. maps high/medium gaps to bead IDs, and
//! 3. references real implementation anchor paths in-repo.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

const MATRIX_RELATIVE_PATH: &str = "docs/design/ntm-fcp-traceability-matrix.json";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root path should resolve")
}

fn matrix_path(root: &Path) -> PathBuf {
    std::env::var("FT_TRACEABILITY_MATRIX_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| root.join(MATRIX_RELATIVE_PATH))
}

fn load_matrix(path: &Path) -> Value {
    let raw = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read matrix file {}: {err}", path.display()));
    serde_json::from_str::<Value>(&raw)
        .unwrap_or_else(|err| panic!("failed to parse matrix JSON {}: {err}", path.display()))
}

fn object_field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, String> {
    value
        .as_object()
        .ok_or_else(|| "top-level matrix must be a JSON object".to_string())?
        .get(key)
        .ok_or_else(|| format!("missing required field `{key}`"))
}

fn string_field<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    object_field(value, key)?
        .as_str()
        .ok_or_else(|| format!("field `{key}` must be a string"))
}

fn array_field<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], String> {
    object_field(value, key)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("field `{key}` must be an array"))
}

fn validate_matrix_schema(matrix: &Value, root: &Path) -> Result<(), String> {
    let schema_version = string_field(matrix, "schema_version")?;
    if schema_version.trim().is_empty() {
        return Err("schema_version must be non-empty".to_string());
    }

    let artifact = string_field(matrix, "artifact")?;
    if artifact != "ntm-fcp-traceability-matrix" {
        return Err(format!(
            "artifact must be `ntm-fcp-traceability-matrix`, got `{artifact}`"
        ));
    }

    let bead_id = string_field(matrix, "bead_id")?;
    if !bead_id.starts_with("ft-") {
        return Err(format!("bead_id must start with `ft-`, got `{bead_id}`"));
    }

    let _generated_at = string_field(matrix, "generated_at_utc")?;
    let entries = array_field(matrix, "entries")?;
    if entries.is_empty() {
        return Err("entries must not be empty".to_string());
    }

    let mut seen_capability_ids = HashSet::new();

    for entry in entries {
        let capability_id = string_field(entry, "capability_id")?;
        if !seen_capability_ids.insert(capability_id.to_string()) {
            return Err(format!("duplicate capability_id `{capability_id}`"));
        }

        let _capability_name = string_field(entry, "capability_name")?;
        let _source_domain = string_field(entry, "source_domain")?;
        let status = string_field(entry, "status")?;
        let gap_severity = string_field(entry, "gap_severity")?;
        let mapped_bead_ids = array_field(entry, "mapped_bead_ids")?;
        let surfaces = array_field(entry, "surfaces")?;
        let anchors = array_field(entry, "implementation_anchors")?;
        let _evidence_notes = string_field(entry, "evidence_notes")?;

        if surfaces.is_empty() {
            return Err(format!(
                "entry `{capability_id}` must list at least one surface"
            ));
        }
        if anchors.is_empty() {
            return Err(format!(
                "entry `{capability_id}` must list at least one implementation anchor"
            ));
        }

        match status {
            "implemented" | "partial" | "gap" => {}
            other => {
                return Err(format!(
                    "entry `{capability_id}` has invalid status `{other}`"
                ));
            }
        }

        match gap_severity {
            "none" | "low" | "medium" | "high" => {}
            other => {
                return Err(format!(
                    "entry `{capability_id}` has invalid gap_severity `{other}`"
                ));
            }
        }

        if matches!(gap_severity, "high" | "medium") && mapped_bead_ids.is_empty() {
            return Err(format!(
                "entry `{capability_id}` has unmapped high/medium gap (mapped_bead_ids empty)"
            ));
        }

        if status == "gap" && gap_severity == "none" {
            return Err(format!(
                "entry `{capability_id}` is status=gap but gap_severity=none"
            ));
        }

        if status == "implemented" && matches!(gap_severity, "medium" | "high") {
            return Err(format!(
                "entry `{capability_id}` is status=implemented but gap_severity={gap_severity}"
            ));
        }

        for bead in mapped_bead_ids {
            let bead_id = bead
                .as_str()
                .ok_or_else(|| format!("entry `{capability_id}` has non-string bead reference"))?;
            if !bead_id.starts_with("ft-") {
                return Err(format!(
                    "entry `{capability_id}` has invalid bead reference `{bead_id}`"
                ));
            }
        }

        for anchor in anchors {
            let anchor_path = anchor
                .as_str()
                .ok_or_else(|| format!("entry `{capability_id}` has non-string anchor path"))?;
            let absolute = root.join(anchor_path);
            if !absolute.exists() {
                return Err(format!(
                    "entry `{capability_id}` anchor does not exist: `{anchor_path}`"
                ));
            }
        }
    }

    Ok(())
}

#[test]
fn traceability_matrix_schema_is_valid() {
    let root = repo_root();
    let path = matrix_path(&root);
    let matrix = load_matrix(&path);
    validate_matrix_schema(&matrix, &root).expect("traceability matrix should validate");
}

#[test]
fn traceability_matrix_anchor_paths_exist() {
    let root = repo_root();
    let path = matrix_path(&root);
    let matrix = load_matrix(&path);
    let entries = array_field(&matrix, "entries").expect("entries array should exist");

    for entry in entries {
        let capability_id =
            string_field(entry, "capability_id").expect("capability_id should exist");
        let anchors = array_field(entry, "implementation_anchors")
            .expect("implementation_anchors should exist");
        for anchor in anchors {
            let rel = anchor
                .as_str()
                .unwrap_or_else(|| panic!("anchor in `{capability_id}` should be a string"));
            let abs = root.join(rel);
            assert!(
                abs.exists(),
                "anchor path for `{capability_id}` does not exist: {}",
                abs.display()
            );
        }
    }
}

#[test]
fn traceability_matrix_validation_detects_unmapped_high_gap() {
    let root = repo_root();
    let path = matrix_path(&root);
    let mut matrix = load_matrix(&path);

    let entries = matrix
        .get_mut("entries")
        .and_then(Value::as_array_mut)
        .expect("entries must be mutable array");
    let first = entries
        .first_mut()
        .and_then(Value::as_object_mut)
        .expect("first entry must be mutable object");

    first.insert("status".to_string(), Value::String("gap".to_string()));
    first.insert(
        "gap_severity".to_string(),
        Value::String("high".to_string()),
    );
    first.insert("mapped_bead_ids".to_string(), Value::Array(Vec::new()));

    let err = validate_matrix_schema(&matrix, &root)
        .expect_err("validator should reject unmapped high/medium gaps");
    assert!(
        err.contains("unmapped high/medium gap"),
        "unexpected error for unmapped gap: {err}"
    );
}
