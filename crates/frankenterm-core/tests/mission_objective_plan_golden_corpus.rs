//! Golden corpus harness for mission objective planning.
//!
//! The corpus freezes redacted planner inputs, retained source-artifact hashes,
//! expected contract fields, and JSON/TOON determinism. It intentionally calls
//! the real side-effect-free planner; no Beads, Agent Mail, RCH, git, or pane
//! mutations happen in this test.

use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::mission_objective_plan::{
    MissionObjectiveActionKind, MissionObjectiveCandidateReadiness, MissionObjectiveCandidateWork,
    MissionObjectiveCapacityPosture, MissionObjectivePlanStatus, MissionObjectivePlannerInput,
    MissionObjectiveRiskLevel, build_mission_objective_plan_surface_data, plan_mission_objective,
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
    if !validator.is_valid(value) {
        let messages = validator
            .iter_errors(value)
            .map(|error| format!("{}: {}", error.instance_path(), error))
            .collect::<Vec<_>>()
            .join("\n");
        panic!("{case_id}: generated plan violates schema fields:\n{messages}");
    }
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path)
        .unwrap_or_else(|err| panic!("failed to read artifact {}: {err}", path.display()));
    hex::encode(Sha256::digest(&bytes))
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

fn integer_contract_field(case_id: &str, value: &Value, role: &str, pointer: &str) -> u64 {
    let number = value
        .pointer(pointer)
        .and_then(Value::as_number)
        .unwrap_or_else(|| panic!("{case_id}: {role} numeric contract field {pointer}"));
    if let Some(unsigned) = number.as_u64() {
        return unsigned;
    }
    if let Some(signed) = number.as_i64() {
        return u64::try_from(signed)
            .unwrap_or_else(|_| panic!("{case_id}: {role} contract field {pointer} is negative"));
    }

    let text = number.to_string();
    let (whole, fraction) = text
        .split_once('.')
        .unwrap_or_else(|| panic!("{case_id}: {role} contract field {pointer} is not integral"));
    assert!(
        !fraction.bytes().any(|digit| digit != b'0'),
        "{case_id}: {role} contract field {pointer} is not integral",
    );
    whole
        .parse::<u64>()
        .unwrap_or_else(|_| panic!("{case_id}: {role} contract field {pointer} is out of range"))
}

fn assert_toon_contract_field_round_trips(
    case_id: &str,
    expected: &Value,
    decoded: &Value,
    pointer: &str,
) {
    let expected_field = expected.pointer(pointer);
    let decoded_field = decoded.pointer(pointer);
    if pointer == "/schema_version" {
        let expected_number = integer_contract_field(case_id, expected, "expected", pointer);
        let decoded_number = integer_contract_field(case_id, decoded, "decoded", pointer);
        assert_eq!(
            decoded_number, expected_number,
            "{case_id}: TOON round-trip changed numeric contract field {pointer}"
        );
        return;
    }

    assert_eq!(
        decoded_field, expected_field,
        "{case_id}: TOON round-trip changed contract field {pointer}"
    );
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
        assert_toon_contract_field_round_trips(case_id, value, &decoded_value, pointer);
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

#[cfg(all(feature = "subprocess-bridge", unix))]
#[test]
fn mission_objective_graph_snapshot_real_reader_plan_and_schema_preserve_decision() {
    use frankenterm_core::beads_bridge::read_bead_work_selection;
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("issues.jsonl");
    let bytes = br#"{"id":"blocked","status":"blocked","priority":0,"issue_type":"bug"}
{"id":"blocked-dependent","status":"open","priority":0,"issue_type":"test","dependencies":[{"issue_id":"blocked-dependent","depends_on_id":"blocked","type":"blocks"}]}
{"id":"ready-docs","status":"open","priority":2,"issue_type":"docs","description":"private-body-schema-negative"}
"#;
    fs::write(&path, bytes).unwrap();
    let hash = hex::encode(Sha256::digest(bytes));
    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let selection = read_bead_work_selection(&path, &hash, 1, now_ms).unwrap();
    assert_eq!(selection.selected_id(), Some("ready-docs"));
    let input =
        MissionObjectivePlannerInput::new(now_ms, "owned-snapshot-test", "choose eligible work")
            .with_bead_work_selection(selection);
    let plan = plan_mission_objective(&input);
    assert_eq!(
        plan.plan_steps[0].target_bead_id.as_deref(),
        Some("ready-docs")
    );
    assert_eq!(plan.plan_status, MissionObjectivePlanStatus::Actionable);
    assert!(!plan.side_effects_executed);
    let value = serde_json::to_value(&plan).unwrap();
    let validator = schema_validator();
    assert_schema_accepts("owned-beads-snapshot", &validator, &value);
    assert_eq!(value["bead_work_selection"]["input_sha256"], hash);
    assert_eq!(
        value["bead_work_selection"]["ordered_ready_ids"],
        serde_json::json!(["ready-docs"])
    );
    assert_eq!(value["bead_work_selection"]["type_population"]["test"], 1);
    assert_eq!(value["bead_work_selection"]["type_population"]["docs"], 1);
    assert!(
        !serde_json::to_string(&value)
            .unwrap()
            .contains("private-body-schema-negative")
    );
    let mut forged_live = value.clone();
    forged_live["bead_work_selection"]["live_database_validated"] = true.into();
    assert!(!validator.is_valid(&forged_live));
    let mut forged_hash = value.clone();
    forged_hash["bead_work_selection"]["input_sha256"] = "not-a-sha256".into();
    assert!(!validator.is_valid(&forged_hash));
    let mut leaked_body = value;
    leaked_body["bead_work_selection"]["candidates"][0]["description"] =
        "private-body-schema-negative".into();
    assert!(!validator.is_valid(&leaked_body));
    println!(
        "MISSION_GRAPH_SCHEMA hash={hash} selected=ready-docs actual_file_reader=true blocked_high_score_excluded=true live_claim_hash_body_negatives_rejected=true"
    );
}

#[test]
fn mission_objective_capacity_admit_defer_matrix_is_deterministic() {
    #[derive(Debug)]
    struct Case {
        name: &'static str,
        posture: MissionObjectiveCapacityPosture,
        expected_status: MissionObjectivePlanStatus,
        expected_action: MissionObjectiveActionKind,
        expected_risk: MissionObjectiveRiskLevel,
        expected_capacity_action: &'static str,
    }

    let cases = [
        Case {
            name: "capacity_admits_ready_work",
            posture: MissionObjectiveCapacityPosture::Admit,
            expected_status: MissionObjectivePlanStatus::Actionable,
            expected_action: MissionObjectiveActionKind::ChooseReadyBead,
            expected_risk: MissionObjectiveRiskLevel::Low,
            expected_capacity_action: "admit",
        },
        Case {
            name: "capacity_defers_ready_work",
            posture: MissionObjectiveCapacityPosture::Defer,
            expected_status: MissionObjectivePlanStatus::WaitingExternal,
            expected_action: MissionObjectiveActionKind::WaitExternal,
            expected_risk: MissionObjectiveRiskLevel::High,
            expected_capacity_action: "defer",
        },
        Case {
            name: "capacity_pause_denies_new_claim",
            posture: MissionObjectiveCapacityPosture::Pause,
            expected_status: MissionObjectivePlanStatus::WaitingExternal,
            expected_action: MissionObjectiveActionKind::WaitExternal,
            expected_risk: MissionObjectiveRiskLevel::High,
            expected_capacity_action: "defer",
        },
    ];

    for case in cases {
        let input = MissionObjectivePlannerInput::new(1_700_000_000_000, "test", case.name)
            .with_candidate(
                MissionObjectiveCandidateWork::new(
                    format!("candidate.{}", case.name),
                    MissionObjectiveCandidateReadiness::ReadyBead,
                )
                .target_bead_id(format!("ft-{}", case.name))
                .capacity_posture(case.posture),
            );

        let plan = plan_mission_objective(&input);
        let replay = plan_mission_objective(&input);

        assert_eq!(
            plan, replay,
            "{}: planner replay must be deterministic",
            case.name
        );
        assert_eq!(plan.plan_status, case.expected_status, "{}", case.name);
        assert_eq!(plan.risk_level, case.expected_risk, "{}", case.name);
        assert!(plan.dry_run, "{}", case.name);
        assert!(!plan.side_effects_executed, "{}", case.name);
        assert!(!plan.raw_pane_content_stored, "{}", case.name);

        let step = plan
            .plan_steps
            .first()
            .unwrap_or_else(|| panic!("{}: expected one candidate step", case.name));
        assert_eq!(step.action_kind, case.expected_action, "{}", case.name);
        assert_eq!(step.risk_level, case.expected_risk, "{}", case.name);

        let value = serde_json::to_value(&plan)
            .unwrap_or_else(|err| panic!("{}: serialize plan artifact: {err}", case.name));
        assert_eq!(
            value
                .pointer("/capacity_admission/action")
                .and_then(Value::as_str),
            Some(case.expected_capacity_action),
            "{}: capacity admission action",
            case.name
        );
        assert_eq!(
            value
                .pointer("/capacity_admission/reason_codes")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or_default(),
            plan.reason_codes.len(),
            "{}: capacity artifact must carry planner reason-code set",
            case.name
        );
    }
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
