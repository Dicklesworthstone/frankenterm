use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use sha2::{Digest, Sha256};

const EXPECTED_IDS: [&str; 7] = [
    "CF-001", "CF-002", "CF-003", "CF-004", "CF-005", "CF-006", "CF-007",
];

const FAMILY_BEADS: [&str; 5] = ["ft-r920m", "ft-n447z", "ft-5bwjf", "ft-9ntud", "ft-rz0eb"];

#[derive(Debug)]
struct InvariantRow {
    id: String,
    predicate: String,
    families: Vec<String>,
    rationale: String,
    beads: Vec<String>,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root exists")
        .to_path_buf()
}

fn invariant_doc_path() -> PathBuf {
    workspace_root().join("docs/contract-families-cross-invariants.md")
}

fn attestation_path() -> PathBuf {
    workspace_root().join("docs/attestations/contracts/cross-family-matrix.json")
}

fn parse_invariant_rows(text: &str) -> Vec<InvariantRow> {
    text.lines()
        .filter(|line| line.starts_with("| CF-"))
        .map(|line| {
            let columns = line.split('|').map(str::trim).collect::<Vec<_>>();
            assert_eq!(
                columns.len(),
                7,
                "invariant row must have five columns: {line}"
            );
            InvariantRow {
                id: columns[1].to_string(),
                predicate: columns[2].trim_matches('`').to_string(),
                families: columns[3]
                    .split(',')
                    .map(str::trim)
                    .map(ToString::to_string)
                    .collect(),
                rationale: columns[4].to_string(),
                beads: columns[5]
                    .split(',')
                    .map(str::trim)
                    .map(ToString::to_string)
                    .collect(),
            }
        })
        .collect()
}

#[allow(clippy::format_collect)]
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn cross_family_invariant_doc_rows_are_parseable_and_referenced() {
    let text = fs::read_to_string(invariant_doc_path()).expect("read invariant doc");
    let rows = parse_invariant_rows(&text);
    assert_eq!(rows.len(), EXPECTED_IDS.len());

    let ids = rows
        .iter()
        .map(|row| row.id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(ids, EXPECTED_IDS.into_iter().collect::<BTreeSet<_>>());

    for row in rows {
        assert!(
            row.predicate.starts_with("[]"),
            "{} predicate must use TLA+-style always form",
            row.id
        );
        assert!(
            row.predicate.contains("=>")
                || row.predicate.contains("\\A")
                || row.predicate.contains("\\E"),
            "{} predicate must contain a TLA+-style connective",
            row.id
        );
        assert!(
            !row.families.is_empty() && !row.beads.is_empty(),
            "{} must name families and bead references",
            row.id
        );
        assert!(
            !row.rationale.is_empty(),
            "{} must carry a one-line rationale",
            row.id
        );
        for bead in row.beads {
            assert!(
                FAMILY_BEADS.contains(&bead.as_str()),
                "{} references unknown family bead {}",
                row.id,
                bead
            );
        }
    }
}

#[test]
fn cross_family_matrix_attestation_matches_invariant_doc() {
    let doc_bytes = fs::read(invariant_doc_path()).expect("read invariant doc");
    let attestation_text = fs::read_to_string(attestation_path()).expect("read attestation");
    let attestation: Value =
        serde_json::from_str(&attestation_text).expect("attestation is valid JSON");

    assert_eq!(attestation["bead_id"], "ft-tf6g3.46");
    assert_eq!(attestation["matrix"]["tuple_count"], 7776);
    assert_eq!(attestation["matrix"]["pass_rate"], 1.0);
    assert_eq!(
        attestation["invariants_sha256"],
        sha256_hex(&doc_bytes),
        "attestation must pin docs/contract-families-cross-invariants.md"
    );

    for id in EXPECTED_IDS {
        assert_eq!(
            attestation["per_invariant_pass_count"][id], 7776,
            "{id} pass count must cover the full matrix"
        );
    }
}
