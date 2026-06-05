use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

const READINESS_REL_PATH: &str = "docs/attestations/proofs/swarm-capacity-readiness.json";
const MANIFEST_REL_PATH: &str = "docs/attestations/manifest.json";
const ENVELOPE_REL_PATH: &str = "docs/attestations/perf/swarm-capacity-envelope.json";
const TARGET_CLASS_GATE_REL_PATH: &str =
    "docs/attestations/proofs/resource-cockpit-target-class.json";

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn load_json(rel_path: &str) -> Value {
    let path = workspace_root().join(rel_path);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse JSON {}: {err}", path.display()))
}

fn object_array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_else(|| panic!("{key} should be an array"))
}

fn string_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{key} should be a string"))
}

fn is_safe_repo_relative_path(rel_path: &str) -> bool {
    if rel_path.is_empty() {
        return false;
    }
    let path = Path::new(rel_path);
    !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir))
}

fn assert_repo_file_exists(rel_path: &str) {
    assert!(
        is_safe_repo_relative_path(rel_path),
        "path must be safe and repo-relative: {rel_path}"
    );
    let path = workspace_root().join(rel_path);
    assert!(path.is_file(), "referenced file is missing: {rel_path}");
}

fn checklist_states(readiness: &Value) -> BTreeSet<&str> {
    object_array(readiness, "checklist_states")
        .iter()
        .map(|state| string_field(state, "state"))
        .collect()
}

fn claim_rows(readiness: &Value) -> &[Value] {
    object_array(readiness, "claim_matrix")
}

#[test]
fn readiness_checklist_distinguishes_all_proof_states() {
    let readiness = load_json(READINESS_REL_PATH);
    assert_eq!(readiness["schema_version"].as_str(), Some("1.0.0"));
    assert_eq!(readiness["kind"].as_str(), Some("swarm-capacity-readiness"));
    assert_eq!(readiness["produced_by_bead"].as_str(), Some("ft-b94bx.10"));
    assert_eq!(
        readiness["overall_status"].as_str(),
        Some("blocked_target_class_not_proven")
    );
    assert_eq!(readiness["raw_pane_content_stored"].as_bool(), Some(false));
    assert_eq!(readiness["live_mutation_allowed"].as_bool(), Some(false));
    assert_eq!(readiness["side_effects_executed"].as_bool(), Some(false));

    let expected = [
        "measured",
        "simulated",
        "skipped",
        "stale",
        "unavailable",
        "production_proven",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert_eq!(checklist_states(&readiness), expected);
}

#[test]
fn claim_matrix_covers_every_capacity_child_claim_once() {
    let readiness = load_json(READINESS_REL_PATH);
    let claims = claim_rows(&readiness);
    assert_eq!(
        claims.len(),
        readiness["summary"]["claim_count"]
            .as_u64()
            .expect("summary claim_count") as usize
    );

    let actual_beads = claims
        .iter()
        .map(|claim| string_field(claim, "bead_id"))
        .collect::<BTreeSet<_>>();
    let expected_beads = [
        "ft-b94bx.1",
        "ft-b94bx.2",
        "ft-b94bx.3",
        "ft-b94bx.4",
        "ft-b94bx.5",
        "ft-b94bx.6",
        "ft-b94bx.7",
        "ft-b94bx.8",
        "ft-b94bx.9",
        "ft-b94bx.10",
        "ft-b94bx.11",
        "ft-b94bx.12",
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();
    assert_eq!(actual_beads, expected_beads);

    let claim_ids = claims
        .iter()
        .map(|claim| string_field(claim, "claim_id"))
        .collect::<BTreeSet<_>>();
    assert_eq!(
        claim_ids.len(),
        claims.len(),
        "claim_id values must be unique"
    );
}

#[test]
fn readiness_summary_counts_match_claim_rows() {
    let readiness = load_json(READINESS_REL_PATH);
    let claims = claim_rows(&readiness);
    for (state, summary_key) in [
        ("measured", "measured_claims"),
        ("simulated", "simulated_claims"),
        ("skipped", "skipped_claims"),
        ("stale", "stale_claims"),
        ("unavailable", "unavailable_claims"),
        ("production_proven", "production_proven_claims"),
    ] {
        let actual = claims
            .iter()
            .filter(|claim| claim["readiness_state"].as_str() == Some(state))
            .count() as u64;
        assert_eq!(
            readiness["summary"][summary_key].as_u64(),
            Some(actual),
            "summary counter {summary_key} drifted"
        );
    }
}

#[test]
fn every_claim_cites_existing_surfaces_tests_and_artifacts() {
    let readiness = load_json(READINESS_REL_PATH);
    let allowed_states = checklist_states(&readiness);
    for claim in claim_rows(&readiness) {
        let claim_id = string_field(claim, "claim_id");
        let readiness_state = string_field(claim, "readiness_state");
        assert!(
            allowed_states.contains(readiness_state),
            "{claim_id} has unknown readiness_state {readiness_state}"
        );

        for key in ["implementation_surfaces", "tests", "retained_artifacts"] {
            let rows = object_array(claim, key);
            assert!(!rows.is_empty(), "{claim_id} has no {key}");
            for row in rows {
                assert_repo_file_exists(string_field(row, "path"));
            }
        }

        let target_state = string_field(&claim["target_class_proof"], "state");
        assert!(
            allowed_states.contains(target_state),
            "{claim_id} has unknown target_class_proof state {target_state}"
        );
        assert_repo_file_exists(string_field(&claim["target_class_proof"], "artifact"));

        if let Some(manifest_path) = claim["release_attestation"]["manifest_path"].as_str() {
            assert_repo_file_exists(manifest_path);
        }
    }
}

#[test]
fn skipped_target_class_artifact_blocks_high_scale_release_claims() {
    let readiness = load_json(READINESS_REL_PATH);
    let envelope = load_json(ENVELOPE_REL_PATH);
    let target_class_gate = load_json(TARGET_CLASS_GATE_REL_PATH);

    assert_eq!(
        target_class_gate["status"].as_str(),
        Some("skipped_not_proven")
    );
    assert_eq!(
        target_class_gate["current_artifact"]["status"].as_str(),
        Some("skipped_not_proven")
    );
    assert_eq!(
        target_class_gate["current_artifact"]["path"].as_str(),
        Some(
            "tests/e2e/artifacts/target-class/linux-x86_64-high-core/20260512T150000Z/summary.json"
        )
    );
    assert_eq!(
        envelope["status"].as_str(),
        Some("blocked_target_class_not_proven")
    );
    assert_eq!(
        readiness["summary"]["target_class_high_scale_claim_allowed"].as_bool(),
        Some(false)
    );
    assert_eq!(
        readiness["summary"]["release_wording_allowed"].as_bool(),
        Some(false)
    );

    for claim in claim_rows(&readiness) {
        let claim_id = string_field(claim, "claim_id");
        assert_ne!(
            claim["target_class_proof"]["state"].as_str(),
            Some("production_proven"),
            "{claim_id} must not promote skipped target-class proof"
        );
        assert_ne!(
            claim["release_attestation"]["state"].as_str(),
            Some("production_proven"),
            "{claim_id} must not promote release wording while target-class proof is skipped"
        );
    }
}

#[test]
fn manifest_hashes_readiness_dashboard_as_robot_contract_evidence() {
    let manifest = load_json(MANIFEST_REL_PATH);
    let slots = object_array(&manifest, "slots");
    let slot = slots
        .iter()
        .find(|slot| slot["path"].as_str() == Some(READINESS_REL_PATH))
        .expect("manifest slot for swarm capacity readiness dashboard");

    assert_eq!(slot["category"].as_str(), Some("proofs/robot-contracts"));
    assert_eq!(slot["media_type"].as_str(), Some("application/json"));
    assert_eq!(slot["produced_by_bead"].as_str(), Some("ft-b94bx.10"));
    let proof_categories = object_array(slot, "proof_categories")
        .iter()
        .filter_map(Value::as_u64)
        .collect::<BTreeSet<_>>();
    assert!(
        proof_categories.contains(&4) && proof_categories.contains(&5),
        "readiness dashboard must cover conformance and quantitative-attestation proof categories"
    );
}
