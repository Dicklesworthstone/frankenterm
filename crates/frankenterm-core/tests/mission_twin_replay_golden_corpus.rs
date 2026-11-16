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
use frankenterm_core::mission_twin_replay::MissionTwinOwnershipSimulationRequest;
use frankenterm_core::mission_twin_replay::{
    MissionTwinCounterfactualRequest, MissionTwinSurfaceActionContract, MissionTwinSurfaceRequest,
    build_mission_twin_replay_surface_data, build_mission_twin_surface_report,
    mission_twin_surface_action_contracts, simulate_mission_twin_counterfactuals,
    simulate_mission_twin_ownership_handoff,
};
use frankenterm_core::mission_twin_snapshot::MissionTwinSnapshotEnvelope;
use frankenterm_core::robot_family_contract::mission_twin_family_contract;
use jsonschema::Validator;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
struct ReplayManifest {
    schema_version: u16,
    contract_id: String,
    source_bead: String,
    corpus_source_bead: String,
    planner_contract_id: String,
    expected_plan_contract_id: String,
    counterfactual_contract_id: String,
    counterfactual_source_bead: String,
    ownership_contract_id: String,
    ownership_source_bead: String,
    surface_contract_id: String,
    surface_source_bead: String,
    step_log_contract_id: String,
    step_log_path: String,
    invalid_fragments_contract_id: String,
    invalid_fragments_path: String,
    static_verification_command: String,
    rust_verification_filter: String,
    remote_rust_verification_command: String,
    surface_actions: Vec<ManifestSurfaceAction>,
    scrub_rules: Vec<ScrubRule>,
    cases: Vec<ReplayCase>,
    counterfactual_cases: Vec<CounterfactualCase>,
    ownership_cases: Vec<OwnershipCase>,
    surface_cases: Vec<SurfaceCase>,
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
    expected_plan_path: String,
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

#[derive(Debug, Deserialize)]
struct CounterfactualCase {
    case_id: String,
    base_case_id: String,
    request: MissionTwinCounterfactualRequest,
    expected: ExpectedCounterfactualFields,
}

#[derive(Debug, Deserialize)]
struct ExpectedCounterfactualFields {
    live_plan_status: String,
    simulated_plan_status: String,
    top_lane_class: String,
    live_blockers_include: Vec<String>,
    unblocked_reason_codes_include: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OwnershipCase {
    case_id: String,
    base_case_id: String,
    request: MissionTwinOwnershipSimulationRequest,
    expected: ExpectedOwnershipFields,
}

#[derive(Debug, Deserialize)]
struct ExpectedOwnershipFields {
    handoff_state: String,
    dirty_overlap_count: usize,
    reservation_overlap_count: usize,
    owner_count: usize,
    next_actions_include: Vec<String>,
    reason_codes_include: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ManifestSurfaceAction {
    action: String,
    robot_command: String,
    cli_command: String,
    mcp_tool_name: String,
    mcp_resource_uri: String,
    response_payload: String,
    read_only: bool,
    idempotent: bool,
}

#[derive(Debug, Deserialize)]
struct SurfaceCase {
    case_id: String,
    base_case_id: String,
    request: MissionTwinSurfaceRequest,
    expected: ExpectedSurfaceFields,
}

#[derive(Debug, Deserialize)]
struct ExpectedSurfaceFields {
    simulated: bool,
    explain_mode: Option<String>,
    explain_matched: bool,
    counterfactual_report: bool,
    ownership_report: bool,
    reason_codes_include: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedPlanArtifact {
    schema_version: u16,
    contract_id: String,
    source_bead: String,
    case_id: String,
    snapshot_path: String,
    plan: ExpectedPlanOutput,
    safety: ExpectedPlanSafety,
}

#[derive(Debug, Deserialize)]
struct ExpectedPlanOutput {
    contract_id: String,
    plan_status: String,
    risk_level: String,
    top_step: ExpectedPlanTopStep,
    reason_codes_include: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedPlanTopStep {
    candidate_id: String,
    action_kind: String,
    status: String,
    proof_lane: String,
}

#[derive(Debug, Deserialize)]
struct ExpectedPlanSafety {
    dry_run: bool,
    side_effects_executed: bool,
    raw_pane_content_stored: bool,
}

#[derive(Debug, Deserialize)]
struct ReplayStepLogEntry {
    schema_version: u16,
    contract_id: String,
    source_bead: String,
    case_id: String,
    snapshot_path: String,
    step_index: u16,
    step_kind: String,
    candidate_id: String,
    action_kind: String,
    status: String,
    proof_lane: String,
    plan_status: String,
    risk_level: String,
    dry_run: bool,
    side_effects_executed: bool,
    raw_pane_content_stored: bool,
    reason_codes: Vec<String>,
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

fn load_expected_plan(case: &ReplayCase) -> ExpectedPlanArtifact {
    let path = workspace_root().join(&case.expected_plan_path);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read expected plan {}: {err}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|err| {
        panic!(
            "failed to deserialize expected plan {}: {err}",
            path.display()
        )
    })
}

fn read_json_lines<T: DeserializeOwned>(path: &Path) -> Vec<T> {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read JSONL {}: {err}", path.display()));
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            Some(serde_json::from_str(trimmed).unwrap_or_else(|err| {
                panic!(
                    "failed to deserialize JSONL {} line {}: {err}",
                    path.display(),
                    index + 1
                )
            }))
        })
        .collect()
}

fn load_replay_step_logs(manifest: &ReplayManifest) -> Vec<ReplayStepLogEntry> {
    read_json_lines(&workspace_root().join(&manifest.step_log_path))
}

fn retained_step_log<'a>(
    case_id: &str,
    step_logs: &'a [ReplayStepLogEntry],
) -> &'a ReplayStepLogEntry {
    step_logs
        .iter()
        .find(|entry| entry.case_id == case_id)
        .unwrap_or_else(|| panic!("{case_id}: missing retained replay step log"))
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
        panic!("{case_id}: replayed plan violates schema fields:\n{messages}");
    }
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path)
        .unwrap_or_else(|err| panic!("failed to read artifact {}: {err}", path.display()));
    hex::encode(Sha256::digest(&bytes))
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

fn invalid_case<'a>(invalid: &'a Value, case_id: &str) -> &'a Value {
    invalid["cases"]
        .as_array()
        .expect("invalid fixture cases is an array")
        .iter()
        .find(|case| case["case_id"].as_str() == Some(case_id))
        .unwrap_or_else(|| panic!("missing invalid fixture case {case_id}"))
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
    assert_eq!(manifest.corpus_source_bead, "ft-u7r37.6");
    assert_eq!(manifest.planner_contract_id, "ft.mission_objective_plan.v1");
    assert_eq!(
        manifest.expected_plan_contract_id,
        "ft.mission_twin_replay.expected_plan.v1"
    );
    assert_eq!(
        manifest.counterfactual_contract_id,
        "ft.mission_twin_counterfactual_replay.v1"
    );
    assert_eq!(manifest.counterfactual_source_bead, "ft-u7r37.3");
    assert_eq!(
        manifest.ownership_contract_id,
        "ft.mission_twin_ownership_handoff.v1"
    );
    assert_eq!(manifest.ownership_source_bead, "ft-u7r37.4");
    assert_eq!(
        manifest.surface_contract_id,
        "ft.mission_twin.robot_mcp_cli_surface.v1"
    );
    assert_eq!(manifest.surface_source_bead, "ft-u7r37.5");
    assert_eq!(
        manifest.step_log_contract_id,
        "ft.mission_twin_replay.step_log.v1"
    );
    assert_eq!(
        manifest.invalid_fragments_contract_id,
        "ft.mission_twin_replay.invalid_fragments.v1"
    );
    assert_eq!(
        manifest.static_verification_command,
        "bash tests/e2e/test_mission_twin_replay_contract.sh"
    );
    assert_eq!(
        manifest.rust_verification_filter,
        "cargo test -p frankenterm-core --test mission_twin_replay_golden_corpus -- --nocapture"
    );
    assert!(
        manifest
            .remote_rust_verification_command
            .contains("RCH_REQUIRE_REMOTE=1"),
        "remote verification command must require RCH"
    );
    assert_eq!(manifest.cases.len(), 6);
    assert_eq!(manifest.counterfactual_cases.len(), 4);
    assert_eq!(manifest.ownership_cases.len(), 2);
    assert_eq!(manifest.surface_actions.len(), 4);
    assert_eq!(manifest.surface_cases.len(), 4);

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
    let step_logs = load_replay_step_logs(&manifest);
    assert_eq!(
        step_logs.len(),
        manifest.cases.len(),
        "retained replay step log must cover every corpus case"
    );

    for case in &manifest.cases {
        let expected_plan = load_expected_plan(case);
        assert_eq!(expected_plan.schema_version, 1);
        assert_eq!(
            expected_plan.contract_id,
            manifest.expected_plan_contract_id
        );
        assert_eq!(expected_plan.source_bead, manifest.corpus_source_bead);
        assert_eq!(expected_plan.case_id, case.case_id);
        assert_eq!(expected_plan.snapshot_path, case.snapshot_path);
        assert_eq!(expected_plan.plan.contract_id, manifest.planner_contract_id);
        assert_eq!(expected_plan.plan.plan_status, case.expected.plan_status);
        assert_eq!(expected_plan.plan.risk_level, case.expected.risk_level);
        assert_eq!(
            expected_plan.plan.top_step.candidate_id,
            case.expected.top_step_candidate_id
        );
        assert_eq!(
            expected_plan.plan.top_step.action_kind,
            case.expected.top_step_action_kind
        );
        assert_eq!(
            expected_plan.plan.top_step.status,
            case.expected.top_step_status
        );
        assert_eq!(
            expected_plan.plan.top_step.proof_lane,
            case.expected.top_step_proof_lane
        );
        assert_eq!(
            expected_plan.plan.reason_codes_include,
            case.expected.reason_codes_include
        );

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
            expected_plan.plan.plan_status,
            "{}: plan_status golden field changed",
            case.case_id
        );
        assert_eq!(
            serialize_enum(surface.risk_level),
            expected_plan.plan.risk_level,
            "{}: risk_level golden field changed",
            case.case_id
        );

        let step = top_step(&surface);
        assert_eq!(
            step.candidate_id, expected_plan.plan.top_step.candidate_id,
            "{}: top step candidate changed",
            case.case_id
        );
        assert_eq!(
            serialize_enum(step.action_kind),
            expected_plan.plan.top_step.action_kind,
            "{}: top step action kind changed",
            case.case_id
        );
        assert_eq!(
            serialize_enum(step.status),
            expected_plan.plan.top_step.status,
            "{}: top step status changed",
            case.case_id
        );
        assert_eq!(
            serialize_enum(step.proof_lane),
            expected_plan.plan.top_step.proof_lane,
            "{}: top step proof lane changed",
            case.case_id
        );

        for reason_code in &expected_plan.plan.reason_codes_include {
            assert!(
                surface.reason_codes.iter().any(|code| code == reason_code),
                "{}: missing expected reason code {reason_code}",
                case.case_id
            );
        }

        assert_eq!(
            surface.dry_run, expected_plan.safety.dry_run,
            "{}: replay must stay dry-run",
            case.case_id
        );
        assert_eq!(
            surface.side_effects_executed, expected_plan.safety.side_effects_executed,
            "{}: replay must not execute side effects",
            case.case_id
        );
        assert_eq!(
            surface.raw_pane_content_stored, expected_plan.safety.raw_pane_content_stored,
            "{}: replay must not store raw pane content",
            case.case_id
        );

        let retained_log = retained_step_log(&case.case_id, &step_logs);
        assert_eq!(retained_log.schema_version, 1);
        assert_eq!(retained_log.contract_id, manifest.step_log_contract_id);
        assert_eq!(retained_log.source_bead, manifest.corpus_source_bead);
        assert_eq!(retained_log.snapshot_path, case.snapshot_path);
        assert_eq!(retained_log.step_index, 0);
        assert_eq!(retained_log.step_kind, "top_plan_step");
        assert_eq!(retained_log.candidate_id, step.candidate_id);
        assert_eq!(retained_log.action_kind, serialize_enum(step.action_kind));
        assert_eq!(retained_log.status, serialize_enum(step.status));
        assert_eq!(retained_log.proof_lane, serialize_enum(step.proof_lane));
        assert_eq!(
            retained_log.plan_status,
            serialize_enum(surface.plan_status)
        );
        assert_eq!(retained_log.risk_level, serialize_enum(surface.risk_level));
        assert_eq!(retained_log.dry_run, surface.dry_run);
        assert_eq!(
            retained_log.side_effects_executed,
            surface.side_effects_executed
        );
        assert_eq!(
            retained_log.raw_pane_content_stored,
            surface.raw_pane_content_stored
        );
        assert_eq!(
            retained_log.reason_codes,
            expected_plan.plan.reason_codes_include
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

#[test]
fn mission_twin_replay_negative_fixture_inventory_covers_safety_edges() {
    let manifest = load_manifest();
    let invalid = read_json(&workspace_root().join(&manifest.invalid_fragments_path));

    assert_eq!(invalid["schema_version"].as_u64(), Some(1));
    assert_eq!(
        invalid["contract_id"].as_str(),
        Some(manifest.invalid_fragments_contract_id.as_str())
    );
    assert_eq!(
        invalid["source_bead"].as_str(),
        Some(manifest.corpus_source_bead.as_str())
    );
    assert_eq!(
        invalid["manifest_path"].as_str(),
        Some("fixtures/mission-twin/replay/manifest.json")
    );

    let case_ids = invalid["cases"]
        .as_array()
        .expect("invalid fixture cases is an array")
        .iter()
        .map(|case| {
            case["case_id"]
                .as_str()
                .expect("invalid fixture case id is string")
                .to_string()
        })
        .collect::<Vec<_>>();
    let expected_case_ids = [
        "raw-pane-text",
        "destructive-suggestion",
        "ambiguous-artifact-path",
        "stale-timestamp",
        "live-mutation-attempt",
    ]
    .into_iter()
    .map(str::to_string)
    .collect::<Vec<_>>();
    assert_eq!(case_ids, expected_case_ids);

    let raw = invalid_case(&invalid, "raw-pane-text");
    assert_eq!(
        raw["invalid_fragment"]["raw_pane_content_stored"].as_bool(),
        Some(true)
    );
    assert!(
        raw["reason_codes"]
            .as_array()
            .expect("raw reason codes")
            .iter()
            .any(|reason| reason.as_str() == Some("mission_twin_replay.invalid.raw_pane_text"))
    );

    let destructive = invalid_case(&invalid, "destructive-suggestion");
    assert!(
        destructive["invalid_fragment"]["suggested_action"]
            .as_str()
            .expect("destructive suggested action")
            .contains("delete tracked source files")
    );

    let ambiguous = invalid_case(&invalid, "ambiguous-artifact-path");
    let artifact_paths = ambiguous["invalid_fragment"]["artifact_paths"]
        .as_array()
        .expect("ambiguous artifact paths");
    assert!(
        artifact_paths
            .iter()
            .any(|path| path.as_str() == Some("fixtures/mission-twin/replay/*"))
    );
    assert!(
        artifact_paths
            .iter()
            .any(|path| path.as_str().is_some_and(|path| path.starts_with('/')))
    );
    assert!(
        artifact_paths
            .iter()
            .any(|path| path.as_str().is_some_and(|path| path.starts_with("../")))
    );

    let stale = invalid_case(&invalid, "stale-timestamp");
    assert_eq!(
        stale["invalid_fragment"]["generated_at_ms"].as_u64(),
        Some(1)
    );
    assert_eq!(
        stale["invalid_fragment"]["freshness_state"].as_str(),
        Some("fresh")
    );

    let mutation = invalid_case(&invalid, "live-mutation-attempt");
    assert_eq!(
        mutation["invalid_fragment"]["live_attempt"].as_bool(),
        Some(true)
    );
    assert_eq!(
        mutation["invalid_fragment"]["dry_run"].as_bool(),
        Some(false)
    );
    assert_eq!(
        mutation["invalid_fragment"]["side_effects_executed"].as_bool(),
        Some(true)
    );
}

#[test]
fn mission_twin_counterfactual_corpus_compares_live_and_simulated_fields() {
    let manifest = load_manifest();
    assert_eq!(
        manifest.counterfactual_contract_id,
        "ft.mission_twin_counterfactual_replay.v1"
    );
    assert_eq!(manifest.counterfactual_source_bead, "ft-u7r37.3");

    for case in &manifest.counterfactual_cases {
        let base_case = manifest
            .cases
            .iter()
            .find(|candidate| candidate.case_id == case.base_case_id)
            .unwrap_or_else(|| panic!("{}: missing base case {}", case.case_id, case.base_case_id));
        let snapshot = load_snapshot(base_case);
        let report =
            simulate_mission_twin_counterfactuals(&[snapshot], std::slice::from_ref(&case.request))
                .unwrap_or_else(|err| {
                    panic!("{}: counterfactual replay failed: {err}", case.case_id)
                });

        assert_eq!(report.contract_id, manifest.counterfactual_contract_id);
        assert_eq!(report.source_bead, manifest.counterfactual_source_bead);
        assert!(
            report.simulated,
            "{}: report must be simulated",
            case.case_id
        );
        assert!(
            !report.side_effects_executed,
            "{}: counterfactual must not execute side effects",
            case.case_id
        );
        assert!(
            !report.raw_pane_content_stored,
            "{}: counterfactual must not store raw pane content",
            case.case_id
        );
        assert_eq!(
            serialize_enum(report.live_plan.plan_status),
            case.expected.live_plan_status,
            "{}: live plan status changed",
            case.case_id
        );

        let simulated = report
            .counterfactual_plans
            .first()
            .unwrap_or_else(|| panic!("{}: missing simulated plan", case.case_id));
        assert_eq!(
            serialize_enum(simulated.plan_status),
            case.expected.simulated_plan_status,
            "{}: simulated plan status changed",
            case.case_id
        );
        assert_eq!(
            serialize_enum(
                simulated
                    .proof_lane_broker
                    .decisions
                    .first()
                    .unwrap_or_else(|| panic!("{}: missing broker decision", case.case_id))
                    .lane_class
            ),
            case.expected.top_lane_class,
            "{}: top proof lane class changed",
            case.case_id
        );

        for blocker in &case.expected.live_blockers_include {
            assert!(
                simulated
                    .live_execution_blocked_by
                    .iter()
                    .any(|item| item == blocker),
                "{}: missing live blocker {blocker}",
                case.case_id
            );
        }
        for reason_code in &case.expected.unblocked_reason_codes_include {
            assert!(
                simulated
                    .unblocked_reason_codes
                    .iter()
                    .any(|item| item == reason_code),
                "{}: missing unblocked reason code {reason_code}",
                case.case_id
            );
        }

        let report_value = serde_json::to_value(&report).unwrap_or_else(|err| {
            panic!("{}: serialize counterfactual report: {err}", case.case_id)
        });
        let json_once = serde_json::to_string_pretty(&report_value).unwrap_or_else(|err| {
            panic!("{}: counterfactual JSON encode failed: {err}", case.case_id)
        });
        let json_twice = serde_json::to_string_pretty(&report_value).unwrap_or_else(|err| {
            panic!(
                "{}: counterfactual JSON re-encode failed: {err}",
                case.case_id
            )
        });
        assert_eq!(
            json_once, json_twice,
            "{}: counterfactual JSON must be deterministic",
            case.case_id
        );
    }
}

#[test]
fn mission_twin_ownership_corpus_matches_active_owner_and_dirty_overlap_golden_fields() {
    let manifest = load_manifest();
    assert_eq!(
        manifest.ownership_contract_id,
        "ft.mission_twin_ownership_handoff.v1"
    );
    assert_eq!(manifest.ownership_source_bead, "ft-u7r37.4");

    for case in &manifest.ownership_cases {
        let base_case = manifest
            .cases
            .iter()
            .find(|candidate| candidate.case_id == case.base_case_id)
            .unwrap_or_else(|| panic!("{}: missing base case {}", case.case_id, case.base_case_id));
        let snapshot = load_snapshot(base_case);
        let report = simulate_mission_twin_ownership_handoff(&[snapshot], &case.request)
            .unwrap_or_else(|err| panic!("{}: ownership simulation failed: {err}", case.case_id));

        assert_eq!(report.contract_id, manifest.ownership_contract_id);
        assert_eq!(report.source_bead, manifest.ownership_source_bead);
        assert!(
            report.simulated,
            "{}: ownership report must be simulated",
            case.case_id
        );
        assert!(
            !report.side_effects_executed,
            "{}: ownership simulation must not execute side effects",
            case.case_id
        );
        assert!(
            !report.raw_pane_content_stored,
            "{}: ownership simulation must not store raw pane content",
            case.case_id
        );
        assert_eq!(
            serialize_enum(report.handoff_state),
            case.expected.handoff_state,
            "{}: handoff state changed",
            case.case_id
        );
        assert_eq!(
            report.dirty_overlaps.len(),
            case.expected.dirty_overlap_count,
            "{}: dirty overlap count changed",
            case.case_id
        );
        assert_eq!(
            report.reservation_overlaps.len(),
            case.expected.reservation_overlap_count,
            "{}: reservation overlap count changed",
            case.case_id
        );
        assert_eq!(
            report.owner_summaries.len(),
            case.expected.owner_count,
            "{}: owner count changed",
            case.case_id
        );

        let action_names = report
            .next_actions
            .iter()
            .map(serialize_enum)
            .collect::<Vec<_>>();
        for action in &case.expected.next_actions_include {
            assert!(
                action_names.iter().any(|item| item == action),
                "{}: missing expected next action {action}",
                case.case_id
            );
        }
        for reason_code in &case.expected.reason_codes_include {
            assert!(
                report.reason_codes.iter().any(|item| item == reason_code),
                "{}: missing ownership reason code {reason_code}",
                case.case_id
            );
        }

        let report_value = serde_json::to_value(&report)
            .unwrap_or_else(|err| panic!("{}: serialize ownership report: {err}", case.case_id));
        let json_once = serde_json::to_string_pretty(&report_value)
            .unwrap_or_else(|err| panic!("{}: ownership JSON encode failed: {err}", case.case_id));
        let json_twice = serde_json::to_string_pretty(&report_value).unwrap_or_else(|err| {
            panic!("{}: ownership JSON re-encode failed: {err}", case.case_id)
        });
        assert_eq!(
            json_once, json_twice,
            "{}: ownership JSON must be deterministic",
            case.case_id
        );
    }
}

#[test]
fn mission_twin_surface_catalog_matches_robot_and_mcp_contracts() {
    let manifest = load_manifest();
    assert_eq!(
        manifest.surface_contract_id,
        "ft.mission_twin.robot_mcp_cli_surface.v1"
    );
    assert_eq!(manifest.surface_source_bead, "ft-u7r37.5");

    let surface_catalog = mission_twin_surface_action_contracts();
    let robot_family = mission_twin_family_contract();
    assert!(
        robot_family.validate().is_empty(),
        "mission_twin robot family contract must validate"
    );
    assert_eq!(surface_catalog.len(), manifest.surface_actions.len());
    assert_eq!(robot_family.action_count(), manifest.surface_actions.len());

    let descriptors = robot_family.mcp_tool_descriptors();
    for action in &manifest.surface_actions {
        let surface_action = surface_catalog
            .iter()
            .find(|candidate| serialize_enum(candidate.action) == action.action)
            .unwrap_or_else(|| panic!("missing surface action {}", action.action));
        assert_surface_action_contract_matches_manifest(surface_action, action);

        let robot_action = robot_family
            .action(&action.action)
            .unwrap_or_else(|| panic!("missing robot family action {}", action.action));
        assert_eq!(robot_action.robot_command, action.robot_command);
        assert_eq!(robot_action.mcp_tool_name, action.mcp_tool_name);
        assert!(robot_action.side_effects.is_read_only());
        assert!(
            robot_action
                .response_schema
                .fields
                .iter()
                .any(|field| field.name == "artifact_paths" && field.required),
            "{} must expose retained artifact paths",
            action.action
        );

        assert!(
            descriptors
                .iter()
                .any(|descriptor| descriptor.name == action.mcp_tool_name),
            "{} missing MCP descriptor",
            action.mcp_tool_name
        );
    }
}

#[test]
fn mission_twin_surface_cases_match_reviewed_golden_fields() {
    let manifest = load_manifest();

    for case in &manifest.surface_cases {
        let base_case = manifest
            .cases
            .iter()
            .find(|candidate| candidate.case_id == case.base_case_id)
            .unwrap_or_else(|| panic!("{}: missing base case {}", case.case_id, case.base_case_id));
        let snapshot = load_snapshot(base_case);
        let report = build_mission_twin_surface_report(&[snapshot], &case.request)
            .unwrap_or_else(|err| panic!("{}: surface build failed: {err}", case.case_id));

        assert_eq!(report.contract_id, manifest.surface_contract_id);
        assert_eq!(report.source_bead, manifest.surface_source_bead);
        assert_eq!(report.schema_version, 1);
        assert_eq!(
            serialize_enum(report.action),
            serialize_enum(case.request.action),
            "{}: report action drifted",
            case.case_id
        );
        let action = manifest
            .surface_actions
            .iter()
            .find(|candidate| candidate.action == serialize_enum(case.request.action))
            .unwrap_or_else(|| panic!("{}: missing manifest action", case.case_id));
        assert_surface_action_contract_matches_manifest(&report.action_contract, action);

        assert_eq!(
            report.simulated, case.expected.simulated,
            "{}: simulated flag drifted",
            case.case_id
        );
        assert!(
            !report.side_effects_executed,
            "{}: surface must not execute side effects",
            case.case_id
        );
        assert!(
            !report.raw_pane_content_stored,
            "{}: surface must not store raw pane content",
            case.case_id
        );
        assert!(
            report.plan_surface.dry_run,
            "{}: embedded plan must stay dry-run",
            case.case_id
        );
        assert!(
            !report.plan_surface.side_effects_executed,
            "{}: embedded plan must not execute side effects",
            case.case_id
        );
        assert_eq!(
            report.counterfactual_report.is_some(),
            case.expected.counterfactual_report,
            "{}: counterfactual report presence changed",
            case.case_id
        );
        assert_eq!(
            report.ownership_report.is_some(),
            case.expected.ownership_report,
            "{}: ownership report presence changed",
            case.case_id
        );

        match (
            case.expected.explain_mode.as_deref(),
            report.plan_surface.explain.as_ref(),
        ) {
            (None, None) => {}
            (Some(expected_mode), Some(explain)) => {
                assert_eq!(
                    serialize_enum(explain.mode),
                    expected_mode,
                    "{}: explain mode drifted",
                    case.case_id
                );
                assert_eq!(
                    explain.matched, case.expected.explain_matched,
                    "{}: explain match flag drifted",
                    case.case_id
                );
            }
            _ => panic!("{}: explain presence drifted", case.case_id),
        }

        for path in &case.request.snapshot_paths {
            assert!(
                report.snapshot_paths.iter().any(|item| item == path),
                "{}: missing request snapshot path {path}",
                case.case_id
            );
            assert!(
                report.artifact_paths.iter().any(|item| item == path),
                "{}: missing artifact path {path}",
                case.case_id
            );
        }
        for reason_code in &case.expected.reason_codes_include {
            assert!(
                report.reason_codes.iter().any(|item| item == reason_code),
                "{}: missing surface reason code {reason_code}",
                case.case_id
            );
        }
        assert!(
            report
                .forbidden_actions
                .iter()
                .any(|action| action == "local_cargo_proof"),
            "{}: forbidden action contract must mention local_cargo_proof",
            case.case_id
        );

        let report_value = serde_json::to_value(&report)
            .unwrap_or_else(|err| panic!("{}: serialize surface report: {err}", case.case_id));
        let json_once = serde_json::to_string_pretty(&report_value)
            .unwrap_or_else(|err| panic!("{}: surface JSON encode failed: {err}", case.case_id));
        let json_twice = serde_json::to_string_pretty(&report_value)
            .unwrap_or_else(|err| panic!("{}: surface JSON re-encode failed: {err}", case.case_id));
        assert_eq!(
            json_once, json_twice,
            "{}: surface JSON must be deterministic",
            case.case_id
        );

        let toon_once = encode_toon(&report_value);
        let toon_twice = encode_toon(&report_value);
        assert_eq!(
            toon_once, toon_twice,
            "{}: surface TOON must be deterministic",
            case.case_id
        );
    }
}

fn assert_surface_action_contract_matches_manifest(
    surface_action: &MissionTwinSurfaceActionContract,
    manifest_action: &ManifestSurfaceAction,
) {
    assert_eq!(
        serialize_enum(surface_action.action),
        manifest_action.action
    );
    assert_eq!(surface_action.robot_command, manifest_action.robot_command);
    assert_eq!(surface_action.cli_command, manifest_action.cli_command);
    assert_eq!(surface_action.mcp_tool_name, manifest_action.mcp_tool_name);
    assert_eq!(
        surface_action.mcp_resource_uri,
        manifest_action.mcp_resource_uri
    );
    assert_eq!(
        surface_action.response_payload,
        manifest_action.response_payload
    );
    assert_eq!(surface_action.read_only, manifest_action.read_only);
    assert_eq!(surface_action.idempotent, manifest_action.idempotent);
}
