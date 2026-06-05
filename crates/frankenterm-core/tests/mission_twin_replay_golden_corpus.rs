//! Golden corpus harness for deterministic mission-twin replay.
//!
//! The corpus replays retained mission-twin snapshot fixtures through the real
//! side-effect-free adapter and freezes the top-level plan outcome, ranking
//! decision, reason-code preservation, JSON determinism, and TOON determinism.

use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::mission_objective_plan::{
    MissionObjectivePlanStep, MissionObjectivePlanSurfaceData,
};
use frankenterm_core::mission_twin_replay::build_mission_twin_replay_surface_data;
use frankenterm_core::mission_twin_snapshot::MissionTwinSnapshotEnvelope;
use jsonschema::Validator;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
struct ReplayManifest {
    schema_version: u16,
    contract_id: String,
    source_bead: String,
    planner_contract_id: String,
    scrub_rules: Vec<ScrubRule>,
    cases: Vec<ReplayCase>,
}

#[derive(Debug, Deserialize)]
struct ScrubRule {
    field: String,
    replacement: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct ReplayCase {
    case_id: String,
    snapshot_path: String,
    snapshot_sha256: String,
    expected: ExpectedReplayFields,
}

#[derive(Debug, Deserialize)]
struct ExpectedReplayFields {
    plan_status: String,
    risk_level: String,
    top_step_candidate_id: String,
    top_step_action_kind: String,
    top_step_status: String,
    top_step_proof_lane: String,
    reason_codes_include: Vec<String>,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn replay_dir() -> PathBuf {
    workspace_root()
        .join("fixtures")
        .join("mission-twin")
        .join("replay")
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read JSON {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse JSON {}: {err}", path.display()))
}

fn load_manifest() -> ReplayManifest {
    let path = replay_dir().join("manifest.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read manifest {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse manifest {}: {err}", path.display()))
}

fn load_snapshot(case: &ReplayCase) -> MissionTwinSnapshotEnvelope {
    let path = workspace_root().join(&case.snapshot_path);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read snapshot {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to deserialize snapshot {}: {err}", path.display()))
}

fn schema_validator() -> Validator {
    let schema = read_json(
        &workspace_root()
            .join("docs")
            .join("json-schema")
            .join("ft-mission-objective-plan.json"),
    );
    jsonschema::draft202012::options()
        .build(&schema)
        .expect("mission objective plan schema compiles")
}

fn assert_schema_accepts(case_id: &str, validator: &Validator, value: &Value) {
    if let Err(errors) = validator.validate(value) {
        let messages = errors
            .map(|error| format!("{}: {}", error.instance_path, error))
            .collect::<Vec<_>>()
            .join("\n");
        panic!("{case_id}: replayed plan violates schema fields:\n{messages}");
    }
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path)
        .unwrap_or_else(|err| panic!("failed to read artifact {}: {err}", path.display()));
    let digest = Sha256::digest(&bytes);
    format!("{digest:x}")
}

fn serialize_enum<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .expect("enum serializes")
        .as_str()
        .expect("enum serializes to string")
        .to_string()
}

fn top_step(surface: &MissionObjectivePlanSurfaceData) -> &MissionObjectivePlanStep {
    surface
        .plan
        .plan_steps
        .first()
        .or_else(|| surface.plan.fallback_steps.first())
        .expect("replay plan includes a top step")
}

fn encode_toon(value: &Value) -> String {
    toon_rust::encode(value.clone(), None)
}

#[test]
fn mission_twin_replay_corpus_matches_reviewed_golden_fields() {
    let manifest = load_manifest();
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(
        manifest.contract_id,
        "ft.mission_twin_replay.golden_corpus.v1"
    );
    assert_eq!(manifest.source_bead, "ft-u7r37.2");
    assert_eq!(manifest.planner_contract_id, "ft.mission_objective_plan.v1");
    assert_eq!(manifest.cases.len(), 6);

    for required_rule in [
        "generated_at_ms",
        "workspace_root",
        "elapsed_ms",
        "worker_id",
        "machine_id",
    ] {
        assert!(
            manifest
                .scrub_rules
                .iter()
                .any(|rule| rule.field == required_rule
                    && !rule.replacement.is_empty()
                    && !rule.reason.is_empty()),
            "manifest missing scrub rule for {required_rule}"
        );
    }

    let validator = schema_validator();

    for case in &manifest.cases {
        let snapshot_path = workspace_root().join(&case.snapshot_path);
        assert_eq!(
            sha256_file(&snapshot_path),
            case.snapshot_sha256,
            "{}: snapshot fixture hash drift for {}",
            case.case_id,
            case.snapshot_path
        );

        let snapshot = load_snapshot(case);
        let surface = build_mission_twin_replay_surface_data(&[snapshot], None, None)
            .unwrap_or_else(|err| panic!("{}: replay failed: {err}", case.case_id));
        let plan_value = serde_json::to_value(&surface.plan)
            .unwrap_or_else(|err| panic!("{}: serialize plan: {err}", case.case_id));
        assert_schema_accepts(&case.case_id, &validator, &plan_value);

        assert_eq!(
            serialize_enum(surface.plan_status),
            case.expected.plan_status,
            "{}: plan_status golden field changed",
            case.case_id
        );
        assert_eq!(
            serialize_enum(surface.risk_level),
            case.expected.risk_level,
            "{}: risk_level golden field changed",
            case.case_id
        );

        let step = top_step(&surface);
        assert_eq!(
            step.candidate_id, case.expected.top_step_candidate_id,
            "{}: top step candidate changed",
            case.case_id
        );
        assert_eq!(
            serialize_enum(step.action_kind),
            case.expected.top_step_action_kind,
            "{}: top step action kind changed",
            case.case_id
        );
        assert_eq!(
            serialize_enum(step.status),
            case.expected.top_step_status,
            "{}: top step status changed",
            case.case_id
        );
        assert_eq!(
            serialize_enum(step.proof_lane),
            case.expected.top_step_proof_lane,
            "{}: top step proof lane changed",
            case.case_id
        );

        for reason_code in &case.expected.reason_codes_include {
            assert!(
                surface.reason_codes.iter().any(|code| code == reason_code),
                "{}: missing expected reason code {reason_code}",
                case.case_id
            );
        }

        assert!(
            surface.dry_run,
            "{}: replay must stay dry-run",
            case.case_id
        );
        assert!(
            !surface.side_effects_executed,
            "{}: replay must not execute side effects",
            case.case_id
        );
        assert!(
            !surface.raw_pane_content_stored,
            "{}: replay must not store raw pane content",
            case.case_id
        );

        let surface_value = serde_json::to_value(&surface)
            .unwrap_or_else(|err| panic!("{}: serialize surface: {err}", case.case_id));
        let json_once = serde_json::to_string_pretty(&surface_value)
            .unwrap_or_else(|err| panic!("{}: JSON encode failed: {err}", case.case_id));
        let json_twice = serde_json::to_string_pretty(&surface_value)
            .unwrap_or_else(|err| panic!("{}: JSON re-encode failed: {err}", case.case_id));
        assert_eq!(
            json_once, json_twice,
            "{}: replay surface JSON is not deterministic",
            case.case_id
        );

        let toon_once = encode_toon(&surface_value);
        let toon_twice = encode_toon(&surface_value);
        assert_eq!(
            toon_once, toon_twice,
            "{}: replay surface TOON is not deterministic",
            case.case_id
        );
    }
}
