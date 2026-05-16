//! Golden corpus harness for mission objective planning.
//!
//! The corpus freezes redacted planner inputs, retained source-artifact hashes,
//! expected contract fields, and JSON/TOON determinism. It intentionally calls
//! the real side-effect-free planner; no Beads, Agent Mail, RCH, git, or pane
//! mutations happen in this test.

use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::mission_objective_plan::{
    MissionObjectivePlannerInput, build_mission_objective_plan_surface_data, plan_mission_objective,
};
use jsonschema::Validator;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
struct CorpusManifest {
    schema_version: u16,
    contract_id: String,
    scrub_rules: Vec<ScrubRule>,
    cases: Vec<CorpusCase>,
}

#[derive(Debug, Deserialize)]
struct ScrubRule {
    field: String,
    replacement: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
struct CorpusCase {
    case_id: String,
    input_path: String,
    retained_artifacts: Vec<RetainedArtifact>,
    expected: ExpectedPlanFields,
}

#[derive(Debug, Deserialize)]
struct RetainedArtifact {
    artifact_path: String,
    artifact_kind: String,
    source_command: String,
    exit_code: i32,
    fixture_sha256: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedPlanFields {
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

fn corpus_dir() -> PathBuf {
    workspace_root()
        .join("fixtures")
        .join("mission-planner")
        .join("objective-plan-corpus")
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read JSON {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse JSON {}: {err}", path.display()))
}

fn load_manifest() -> CorpusManifest {
    let path = corpus_dir().join("manifest.json");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read manifest {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse manifest {}: {err}", path.display()))
}

fn load_input(case: &CorpusCase) -> MissionObjectivePlannerInput {
    let path = workspace_root().join(&case.input_path);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read input {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to deserialize input {}: {err}", path.display()))
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
        panic!("{case_id}: generated plan violates schema fields:\n{messages}");
    }
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path)
        .unwrap_or_else(|err| panic!("failed to read artifact {}: {err}", path.display()));
    let digest = Sha256::digest(&bytes);
    format!("{digest:x}")
}

fn string_field(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("missing string field {pointer}"))
        .to_string()
}

fn encode_toon(value: &Value) -> String {
    toon_rust::encode(value.clone(), None)
}

fn assert_toon_round_trips(case_id: &str, value: &Value, encoded: &str) {
    let decoded = toon_rust::try_decode(encoded, None)
        .unwrap_or_else(|err| panic!("{case_id}: TOON decode failed: {err}"));
    let decoded_json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    let decoded_value: Value = serde_json::from_str(&decoded_json)
        .unwrap_or_else(|err| panic!("{case_id}: decoded TOON JSON failed: {err}"));

    for pointer in [
        "/contract_id",
        "/schema_version",
        "/dry_run",
        "/side_effects_executed",
        "/raw_pane_content_stored",
        "/plan_status",
        "/risk_level",
    ] {
        assert_eq!(
            decoded_value.pointer(pointer),
            value.pointer(pointer),
            "{case_id}: TOON round-trip changed contract field {pointer}"
        );
    }
}

fn top_step_field(surface_value: &Value, field: &str) -> String {
    let plan = surface_value
        .get("plan")
        .unwrap_or_else(|| panic!("surface missing plan"));
    let steps = plan
        .get("steps")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("plan missing steps"));
    let first = steps
        .first()
        .unwrap_or_else(|| panic!("plan has no top step in generated golden"));
    string_field(first, &format!("/{field}"))
}

#[test]
fn mission_objective_plan_corpus_matches_reviewed_golden_contract_fields() {
    let manifest = load_manifest();
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(
        manifest.contract_id,
        "ft.mission_objective_plan.golden_corpus.v1"
    );
    assert!(
        manifest.cases.len() >= 7,
        "corpus must cover the seven acceptance scenarios"
    );

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
        for artifact in &case.retained_artifacts {
            assert!(
                !artifact.source_command.trim().is_empty(),
                "{}: retained artifact must log source command",
                case.case_id
            );
            assert!(
                (0..=255).contains(&artifact.exit_code),
                "{}: retained artifact {} records impossible process exit code {}",
                case.case_id,
                artifact.artifact_path,
                artifact.exit_code
            );
            let artifact_path = workspace_root().join(&artifact.artifact_path);
            let actual_hash = sha256_file(&artifact_path);
            assert_eq!(
                actual_hash, artifact.fixture_sha256,
                "{}: fixture hash drift for {}",
                case.case_id, artifact.artifact_path
            );
            match artifact.artifact_kind.as_str() {
                "objective_plan_json" => {
                    assert_eq!(
                        artifact.exit_code, 0,
                        "{}: generated plan artifact {} must come from a successful source command",
                        case.case_id, artifact.artifact_path
                    );
                    let retained_value = read_json(&artifact_path);
                    assert_schema_accepts(&case.case_id, &validator, &retained_value);
                }
                "planner_input_json" => {
                    let retained_value = read_json(&artifact_path);
                    serde_json::from_value::<MissionObjectivePlannerInput>(retained_value)
                        .unwrap_or_else(|err| {
                            panic!(
                                "{}: retained planner input {} failed to deserialize: {err}",
                                case.case_id, artifact.artifact_path
                            )
                        });
                }
                other => panic!(
                    "{}: retained artifact {} has unknown kind {other}",
                    case.case_id, artifact.artifact_path
                ),
            }
        }

        let input = load_input(case);
        let plan = plan_mission_objective(&input);
        let surface = build_mission_objective_plan_surface_data(plan, None, None);
        let surface_value = serde_json::to_value(&surface)
            .unwrap_or_else(|err| panic!("{}: serialize surface: {err}", case.case_id));
        let plan_value = surface_value
            .get("plan")
            .unwrap_or_else(|| panic!("{}: surface missing plan", case.case_id));
        assert_schema_accepts(&case.case_id, &validator, plan_value);

        assert_eq!(
            string_field(&surface_value, "/plan_status"),
            case.expected.plan_status,
            "{}: plan_status golden field changed",
            case.case_id
        );
        assert_eq!(
            string_field(&surface_value, "/risk_level"),
            case.expected.risk_level,
            "{}: risk_level golden field changed",
            case.case_id
        );
        assert_eq!(
            top_step_field(&surface_value, "target"),
            case.expected.top_step_candidate_id,
            "{}: top step target golden field changed",
            case.case_id
        );
        assert_eq!(
            top_step_field(&surface_value, "action_kind"),
            case.expected.top_step_action_kind,
            "{}: top step action_kind golden field changed",
            case.case_id
        );
        assert_eq!(
            top_step_field(&surface_value, "status"),
            case.expected.top_step_status,
            "{}: top step status golden field changed",
            case.case_id
        );
        assert_eq!(
            surface
                .plan
                .plan_steps
                .first()
                .or_else(|| surface.plan.fallback_steps.first())
                .map(|step| serde_json::to_value(step.proof_lane).expect("proof lane"))
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
                .unwrap_or_else(|| "none".to_string()),
            case.expected.top_step_proof_lane,
            "{}: top step proof_lane golden field changed",
            case.case_id
        );

        for reason_code in &case.expected.reason_codes_include {
            assert!(
                surface.reason_codes.iter().any(|code| code == reason_code),
                "{}: missing expected reason code {reason_code}",
                case.case_id
            );
        }

        let json_once = serde_json::to_string_pretty(&surface_value)
            .unwrap_or_else(|err| panic!("{}: JSON encode failed: {err}", case.case_id));
        let json_twice = serde_json::to_string_pretty(&surface_value)
            .unwrap_or_else(|err| panic!("{}: JSON re-encode failed: {err}", case.case_id));
        assert_eq!(
            json_once, json_twice,
            "{}: surface JSON is not deterministic",
            case.case_id
        );

        let toon_once = encode_toon(&surface_value);
        let toon_twice = encode_toon(&surface_value);
        assert_eq!(
            toon_once, toon_twice,
            "{}: surface TOON is not deterministic",
            case.case_id
        );
        assert_toon_round_trips(&case.case_id, &surface_value, &toon_once);
    }
}
