//! Deterministic fixture and conformance checks for the herd-wave v1 contract.
//!
//! The fixture matrix is intentionally metadata-heavy: it documents which
//! synchronized-burst states are stable enough for goldens, which fields may be
//! scrubbed, and which contract requirements each scenario covers.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::PathBuf;

use frankenterm_core::fleet_memory_controller::{
    FleetMemoryTier, FleetMemoryTierBudgetRecord, FleetMemoryTierBudgetSnapshot, FleetPressureTier,
};
use frankenterm_core::latency_stages::{LatencyStage, StagePressure};
use frankenterm_core::priority::PanePriority;
use frankenterm_core::runtime_telemetry::{
    SwarmCapacityAdmissionAction, SwarmCapacityAdmissionControllerState,
};
use frankenterm_core::swarm_scheduler::{
    AdmissionAction, AdmissionRequest, HerdWaveCapacityControllerSnapshot,
    HerdWaveContractSnapshot, HerdWaveDetectionConfig, HerdWaveDryRunPlan, HerdWaveEventKind,
    HerdWaveSignal, MissionCriticality, QueuePressure, SwarmAdmissionController,
    SwarmAdmissionTelemetry, detect_herd_wave_pressure, plan_herd_wave_dry_run_actions,
};
use jsonschema::Validator;
use serde_json::{Value, json};

const FIXTURE_MATRIX: &str = include_str!("fixtures/herd_wave_contract/fixture_matrix.json");
const CONFORMANCE_MATRIX: &str =
    include_str!("fixtures/herd_wave_contract/conformance_matrix.json");

#[derive(Debug)]
struct ScenarioReport {
    scenario_id: String,
    snapshot: HerdWaveContractSnapshot,
    dry_run_plan: HerdWaveDryRunPlan,
    projection: Value,
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("workspace root exists")
        .to_path_buf()
}

fn load_schema_validator() -> Validator {
    let schema_path = workspace_root()
        .join("docs")
        .join("json-schema")
        .join("ft-herd-wave.json");
    let schema_text = fs::read_to_string(&schema_path)
        .unwrap_or_else(|err| panic!("read {}: {err}", schema_path.display()));
    let schema: Value = serde_json::from_str(&schema_text)
        .unwrap_or_else(|err| panic!("parse {}: {err}", schema_path.display()));
    jsonschema::draft202012::options()
        .build(&schema)
        .expect("herd-wave schema compiles")
}

fn fixture_matrix() -> Value {
    serde_json::from_str(FIXTURE_MATRIX).expect("fixture matrix is valid JSON")
}

fn conformance_matrix() -> Value {
    serde_json::from_str(CONFORMANCE_MATRIX).expect("conformance matrix is valid JSON")
}

fn scenario_array(matrix: &Value) -> &[Value] {
    matrix["scenarios"]
        .as_array()
        .expect("fixture matrix has scenarios")
}

fn string_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} must be a string in {value:?}"))
}

fn string_array(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .expect("expected string array")
        .iter()
        .map(|entry| entry.as_str().expect("array entry is a string"))
        .collect()
}

fn enum_json<T>(value: T) -> Value
where
    T: serde::Serialize,
{
    serde_json::to_value(value).expect("enum serializes")
}

fn enum_string<T>(value: T) -> String
where
    T: serde::Serialize,
{
    enum_json(value)
        .as_str()
        .expect("enum serializes as string")
        .to_string()
}

fn parse_kind(kind: &str) -> HerdWaveEventKind {
    match kind {
        "compaction" => HerdWaveEventKind::Compaction,
        "retry" => HerdWaveEventKind::Retry,
        "rate_limit_recovery" => HerdWaveEventKind::RateLimitRecovery,
        "search_burst" => HerdWaveEventKind::SearchBurst,
        "workflow_fanout" => HerdWaveEventKind::WorkflowFanout,
        "wake" => HerdWaveEventKind::Wake,
        "other" => HerdWaveEventKind::Other,
        "mixed" => HerdWaveEventKind::Other,
        _ => panic!("unknown herd-wave kind {kind}"),
    }
}

fn queue_pressure(utilization: f64) -> QueuePressure {
    QueuePressure {
        ready_ratio: 0.10,
        utilization,
        starvation_count: 0,
        failure_rate: 0.0,
        pending_items: 6,
        active_agents: 2,
        total_capacity: 6,
    }
}

fn healthy_tier_budget() -> FleetMemoryTierBudgetSnapshot {
    FleetMemoryTierBudgetSnapshot::from_tiers([FleetMemoryTierBudgetRecord::new(
        FleetMemoryTier::HotResident,
        1_000,
        900,
    )])
}

fn healthy_stage_pressure() -> Vec<StagePressure> {
    vec![StagePressure::compute(
        LatencyStage::PtyCapture,
        500.0,
        1_000.0,
    )]
}

fn healthy_telemetry(
    wave_summary: frankenterm_core::swarm_scheduler::HerdWavePressureSummary,
) -> SwarmAdmissionTelemetry {
    SwarmAdmissionTelemetry::new(
        queue_pressure(0.10),
        FleetPressureTier::Normal,
        healthy_tier_budget(),
        healthy_stage_pressure(),
    )
    .with_herd_wave_pressure(wave_summary)
}

fn missing_telemetry() -> SwarmAdmissionTelemetry {
    SwarmAdmissionTelemetry {
        queue_pressure: None,
        fleet_pressure: None,
        memory_tier_budget: None,
        latency_stage_pressures: None,
        herd_wave_pressure: None,
    }
}

fn request_for(kind: &str) -> AdmissionRequest {
    match kind {
        "background" => AdmissionRequest {
            pane_id: Some(42),
            pane_priority: PanePriority::Background,
            mission_criticality: MissionCriticality::Background,
            work_priority: 9,
            estimated_effort: 1,
            operator_priority_override: false,
        },
        "mission_critical" => AdmissionRequest {
            pane_id: Some(7),
            pane_priority: PanePriority::Critical,
            mission_criticality: MissionCriticality::MissionCritical,
            work_priority: 0,
            estimated_effort: 1,
            operator_priority_override: false,
        },
        "operator_override" => AdmissionRequest {
            pane_id: Some(11),
            pane_priority: PanePriority::Critical,
            mission_criticality: MissionCriticality::MissionCritical,
            work_priority: 0,
            estimated_effort: 1,
            operator_priority_override: true,
        },
        _ => panic!("unknown request kind {kind}"),
    }
}

fn signals_for_scenario(scenario: &Value) -> Vec<HerdWaveSignal> {
    let pane_count = scenario["pane_count"]
        .as_u64()
        .expect("pane_count is present");
    if pane_count == 0 {
        return Vec::new();
    }
    if let Some(kinds) = scenario["signal_kinds"].as_array() {
        return kinds
            .iter()
            .enumerate()
            .map(|(index, kind)| {
                let pane_id = u64::try_from(index + 1).expect("fixture index fits u64");
                let timestamp_ms = 1_770_000_000_000_u64
                    .saturating_add(u64::try_from(index).expect("index fits u64") * 10);
                HerdWaveSignal::pane(
                    pane_id,
                    parse_kind(kind.as_str().expect("signal kind is a string")),
                    timestamp_ms,
                )
            })
            .collect();
    }

    let kind = parse_kind(string_field(scenario, "kind"));
    (0..pane_count)
        .map(|index| {
            HerdWaveSignal::pane(
                index + 1,
                kind,
                1_770_000_000_000_u64.saturating_add(index * 10),
            )
        })
        .collect()
}

fn controller_snapshot_for(
    scenario: &Value,
    action: AdmissionAction,
) -> Option<HerdWaveCapacityControllerSnapshot> {
    match string_field(scenario, "controller_state") {
        "none" => None,
        "pressure_active" => {
            let mut state = SwarmCapacityAdmissionControllerState::default();
            let controller_action = match action {
                AdmissionAction::Admit => SwarmCapacityAdmissionAction::Admit,
                AdmissionAction::Defer => SwarmCapacityAdmissionAction::Defer,
                AdmissionAction::Degrade => SwarmCapacityAdmissionAction::ThrottleCapturePolling,
                AdmissionAction::Shed => SwarmCapacityAdmissionAction::Shed,
            };
            state.record_decision(controller_action, 1_770_000_001_000);
            Some(HerdWaveCapacityControllerSnapshot::from(&state))
        }
        other => panic!("unknown controller state {other}"),
    }
}

fn build_scenario_report(scenario: &Value) -> ScenarioReport {
    let scenario_id = string_field(scenario, "scenario_id").to_string();
    let request = request_for(string_field(scenario, "request"));
    let telemetry_mode = string_field(scenario, "telemetry_mode");
    let signals = signals_for_scenario(scenario);
    let config = HerdWaveDetectionConfig::default();
    let controller = SwarmAdmissionController::default();
    let generated_at_ms = 1_770_000_001_000_u64;
    let max_age_ms = if telemetry_mode == "stale" {
        500
    } else {
        60_000
    };
    let telemetry_generated_at_ms = match telemetry_mode {
        "fresh" => Some(generated_at_ms),
        "stale" => Some(generated_at_ms.saturating_sub(10_000)),
        "missing" => None,
        other => panic!("unknown telemetry mode {other}"),
    };

    let wave_summary = detect_herd_wave_pressure(&signals, &config);
    let telemetry = if telemetry_mode == "missing" {
        missing_telemetry()
    } else {
        healthy_telemetry(wave_summary.clone())
    };
    let decision = controller.evaluate(&request, &telemetry);
    let controller_snapshot = controller_snapshot_for(scenario, decision.action);
    let snapshot = HerdWaveContractSnapshot::from_telemetry(
        generated_at_ms,
        "fixture.herd_wave",
        &telemetry,
        Some(&decision),
        telemetry_generated_at_ms,
        max_age_ms,
    );
    let dry_run_plan = plan_herd_wave_dry_run_actions(
        generated_at_ms,
        &signals,
        &config,
        Some(&decision),
        controller_snapshot.as_ref(),
    );
    let projection = project_contract(&scenario_id, &request, &snapshot, &dry_run_plan);

    ScenarioReport {
        scenario_id,
        snapshot,
        dry_run_plan,
        projection,
    }
}

fn combined_source_freshness(snapshot: &HerdWaveContractSnapshot) -> Value {
    let generated_at_ms = snapshot
        .source_freshness
        .iter()
        .filter_map(|source| source.generated_at_ms)
        .min();
    let freshness_ms = snapshot
        .source_freshness
        .iter()
        .filter_map(|source| source.freshness_ms)
        .max();
    let max_age_ms = snapshot
        .source_freshness
        .iter()
        .map(|source| source.max_age_ms)
        .max()
        .unwrap_or(0);
    let mut reason_codes = Vec::new();
    for source in &snapshot.source_freshness {
        for reason in &source.reason_codes {
            push_string(&mut reason_codes, reason);
        }
    }

    json!({
        "generated_at_ms": generated_at_ms,
        "freshness_ms": freshness_ms,
        "max_age_ms": max_age_ms,
        "evidence_state": enum_string(snapshot.evidence_state),
        "reason_codes": reason_codes
    })
}

fn wave_summary_projection(snapshot: &HerdWaveContractSnapshot) -> Value {
    let summary = &snapshot.wave_summary;
    let mut reason_codes = Vec::new();
    for reason in &snapshot.reason_codes {
        if reason.starts_with("herd_wave.kind.") || reason.starts_with("herd_wave.threshold.") {
            push_string(&mut reason_codes, reason);
        }
    }
    if reason_codes.is_empty() {
        push_string(&mut reason_codes, "herd_wave.admission.healthy");
    }

    json!({
        "detected": summary.detected,
        "event_count": summary.event_count,
        "distinct_panes": summary.distinct_panes,
        "window_ms": summary.window_ms.max(1),
        "first_seen_ms": summary.first_seen_ms,
        "last_seen_ms": summary.last_seen_ms,
        "dominant_kind": summary.dominant_kind.map_or_else(|| "none".to_string(), enum_string),
        "dominant_kind_count": summary.dominant_kind_count,
        "pressure_tier": enum_string(summary.pressure_tier),
        "recommended_stagger_ms": summary.recommended_stagger_ms,
        "cohort_max_stagger_ms": summary.cohort_max_stagger_ms,
        "reason_codes": reason_codes
    })
}

fn priority_projection(request: &AdmissionRequest, snapshot: &HerdWaveContractSnapshot) -> Value {
    json!({
        "protected": snapshot.priority_protection.protected,
        "protection_units": snapshot.priority_protection.protection_units,
        "pane_priority_tier": enum_string(request.pane_priority),
        "work_priority": request.work_priority,
        "mission_critical": request.mission_criticality >= MissionCriticality::Critical,
        "effective_admission_action": snapshot.admission_action.map_or_else(|| "none".to_string(), enum_string),
        "reason_codes": snapshot.priority_protection.reason_codes
    })
}

fn operator_override_projection(request: &AdmissionRequest) -> Value {
    let reason_codes = if request.operator_priority_override {
        vec!["herd_wave.priority.operator_override".to_string()]
    } else {
        Vec::new()
    };
    json!({
        "active": request.operator_priority_override,
        "override_id": if request.operator_priority_override { Some("fixture-operator-override") } else { None },
        "scope": if request.operator_priority_override { Some("fixture.herd_wave") } else { None },
        "approved_by": if request.operator_priority_override { Some("fixture-operator") } else { None },
        "reason_codes": reason_codes
    })
}

fn stagger_plan_projection(scenario_id: &str, dry_run_plan: &HerdWaveDryRunPlan) -> Value {
    Value::Array(
        dry_run_plan
            .calendar
            .iter()
            .map(|entry| {
                json!({
                    "action_id": format!("{scenario_id}:{}", entry.cohort_rank),
                    "pane_id": entry.pane_id,
                    "cohort_rank": entry.cohort_rank,
                    "event_kind": enum_string(entry.kind),
                    "scheduled_after_ms": entry.delay_ms,
                    "admission_action": enum_string(entry.admission_action),
                    "mutation_allowed": false,
                    "reason_codes": entry.reason_codes,
                    "citation_ids": [format!("{scenario_id}:fixture")]
                })
            })
            .collect(),
    )
}

fn next_actions_projection(dry_run_plan: &HerdWaveDryRunPlan) -> Value {
    let actions = if dry_run_plan.operator_next_actions.is_empty() {
        vec!["herd_wave.next.noop".to_string()]
    } else {
        dry_run_plan.operator_next_actions.clone()
    };

    Value::Array(
        actions
            .into_iter()
            .enumerate()
            .map(|(index, reason)| {
                let action_kind = if reason == "herd_wave.next.noop" {
                    "none"
                } else if reason.contains("defer") || reason.contains("preserve_priority") {
                    "pause_assignment"
                } else if reason.contains("degrade") || reason.contains("stagger") {
                    "apply_manual_stagger"
                } else if reason.contains("operator_review") || reason.contains("confirm") {
                    "inspect_telemetry"
                } else {
                    "observe"
                };
                json!({
                    "action_id": format!("next:{index}"),
                    "action_kind": action_kind,
                    "operator_summary": reason,
                    "mutation_allowed": false,
                    "requires_approval": reason.contains("operator_review") || reason.contains("confirm"),
                    "reason_codes": [reason],
                    "citation_ids": []
                })
            })
            .collect(),
    )
}

fn project_contract(
    scenario_id: &str,
    request: &AdmissionRequest,
    snapshot: &HerdWaveContractSnapshot,
    dry_run_plan: &HerdWaveDryRunPlan,
) -> Value {
    json!({
        "schema_version": snapshot.schema_version,
        "contract_id": snapshot.contract_id,
        "generated_at_ms": snapshot.generated_at_ms,
        "source": snapshot.source,
        "source_freshness": combined_source_freshness(snapshot),
        "evidence_state": enum_string(snapshot.evidence_state),
        "overall_state": enum_string(snapshot.overall_state),
        "dominant_kind": snapshot.dominant_kind.map_or_else(|| "none".to_string(), enum_string),
        "event_count": snapshot.event_count,
        "distinct_panes": snapshot.distinct_panes,
        "window_ms": snapshot.window_ms.max(1),
        "pressure_tier": enum_string(snapshot.pressure_tier),
        "admission_action": snapshot.admission_action.map_or_else(|| "none".to_string(), enum_string),
        "reason_codes": snapshot.reason_codes,
        "recommended_stagger_ms": snapshot.recommended_stagger_ms,
        "cohort_max_stagger_ms": snapshot.cohort_max_stagger_ms,
        "wave_summary": wave_summary_projection(snapshot),
        "priority_protection": priority_projection(request, snapshot),
        "operator_override": operator_override_projection(request),
        "stagger_plan": stagger_plan_projection(scenario_id, dry_run_plan),
        "citations": [{
            "citation_id": format!("{scenario_id}:fixture"),
            "source": "crates/frankenterm-core/tests/fixtures/herd_wave_contract/fixture_matrix.json",
            "evidence_state": "simulated",
            "subject_type": "deterministic_fixture",
            "subject_id": scenario_id,
            "generated_at_ms": snapshot.generated_at_ms,
            "artifact_path": "crates/frankenterm-core/tests/fixtures/herd_wave_contract/fixture_matrix.json",
            "reason_codes": ["herd_wave.telemetry.fixture"]
        }],
        "next_actions": next_actions_projection(dry_run_plan),
        "forbidden_actions": [
            "no_agent_mail_restart",
            "no_agent_mail_repair",
            "no_rch_restart_or_drain_without_approval",
            "no_pane_mutation",
            "no_queue_mutation",
            "no_destructive_git_or_filesystem_operation",
            "no_raw_pane_content",
            "no_target_class_claim_without_artifact"
        ],
        "unavailable_sources": snapshot.unavailable_sources,
        "redaction_policy": {
            "raw_pane_content_allowed": false,
            "max_excerpt_chars": 0,
            "secret_redaction_required": true,
            "allowed_citation_subjects": ["counter", "event_id", "hash", "artifact_path", "redacted_label"],
            "reason_codes": ["herd_wave.safety.no_raw_pane_content"]
        },
        "raw_pane_content_stored": false,
        "target_class_hardware_proof": {
            "available": false,
            "cpu_cores": null,
            "memory_gib": null,
            "host_fingerprint": null,
            "rch_worker": null,
            "run_id": null,
            "artifact_path": null,
            "command": null,
            "exit_status": null,
            "measured_window_ms": null,
            "reason_codes": ["herd_wave.safety.no_target_class_artifact"]
        },
        "artifact_paths": [
            "crates/frankenterm-core/tests/fixtures/herd_wave_contract/fixture_matrix.json",
            "crates/frankenterm-core/tests/fixtures/herd_wave_contract/conformance_matrix.json"
        ]
    })
}

fn push_string(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|existing| existing == reason) {
        reasons.push(reason.to_string());
    }
}

fn normalize_integral_toon_numbers(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_integral_toon_numbers(item);
            }
        }
        Value::Object(map) => {
            for nested in map.values_mut() {
                normalize_integral_toon_numbers(nested);
            }
        }
        Value::Number(number) => {
            if let Some(float) = number.as_f64() {
                #[allow(clippy::cast_precision_loss)]
                let max_u64 = u64::MAX as f64;
                if float.fract() == 0.0 && float >= 0.0 && float <= max_u64 {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let as_u64 = float as u64;
                    *value = Value::Number(serde_json::Number::from(as_u64));
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

fn toon_roundtrip_json(value: &Value) -> Value {
    let toon = toon_rust::encode(value.clone(), None);
    let decoded = toon_rust::try_decode(&toon, None).expect("TOON decodes");
    let json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    let mut decoded_value: Value = serde_json::from_str(&json).expect("decoded TOON JSON parses");
    normalize_integral_toon_numbers(&mut decoded_value);
    decoded_value
}

fn contains_any_private_sentinel(text: &str) -> bool {
    [
        "PROMPT_BODY: deploy prod",
        "Bearer ft-5bwjf-private-token",
        "Cookie: ft_session=private",
        "raw pane excerpt with secret",
    ]
    .iter()
    .any(|sentinel| text.contains(sentinel))
}

#[test]
fn fixture_matrix_is_deterministic_and_covers_required_scenarios() {
    let matrix = fixture_matrix();
    assert_eq!(matrix["schema_version"].as_u64(), Some(1));
    assert_eq!(matrix["bead_id"].as_str(), Some("ft-5bwjf.5"));
    assert_eq!(matrix["contract_id"].as_str(), Some("ft.herd_wave.v1"));
    assert_eq!(
        matrix["update_policy"]["default_overwrite_allowed"].as_bool(),
        Some(false)
    );
    assert_eq!(
        matrix["update_policy"]["environment_gate"].as_str(),
        Some("UPDATE_HERD_WAVE_GOLDENS")
    );

    let scenario_ids: BTreeSet<_> = scenario_array(&matrix)
        .iter()
        .map(|scenario| string_field(scenario, "scenario_id"))
        .collect();
    let expected: BTreeSet<_> = [
        "synchronized_compaction",
        "rate_limit_recovery_wave",
        "retry_storm",
        "search_index_burst",
        "mixed_wave",
        "missing_telemetry",
        "stale_evidence",
        "priority_protection",
        "operator_override",
        "cooldown_circuit_active",
        "normal_no_wave_privacy_guard",
    ]
    .into_iter()
    .collect();
    assert_eq!(scenario_ids, expected);

    let golden_rows = matrix["golden_confidence_matrix"]
        .as_array()
        .expect("golden matrix is present");
    for required_surface in ["robot_json", "robot_toon", "doctor_json", "e2e_jsonl"] {
        assert!(
            golden_rows
                .iter()
                .any(|row| row["surface"].as_str() == Some(required_surface)),
            "golden confidence matrix must cover {required_surface}"
        );
    }
    for row in golden_rows {
        let volatility = row["volatility"].as_u64().expect("volatility is present");
        let strategy = string_field(row, "comparison_strategy");
        assert!(
            volatility < 4 || strategy != "exact",
            "volatile golden row must not use exact comparison: {row:?}"
        );
        assert_eq!(row["review_required"].as_bool(), Some(true));
    }
}

#[test]
fn conformance_matrix_has_no_uncovered_must_requirements() {
    let fixtures = fixture_matrix();
    let known_fixture_ids: HashSet<_> = scenario_array(&fixtures)
        .iter()
        .map(|scenario| string_field(scenario, "scenario_id"))
        .collect();
    let matrix = conformance_matrix();
    let requirements = matrix["requirements"]
        .as_array()
        .expect("requirements are present");

    assert!(
        requirements.len() >= 12,
        "conformance matrix should enumerate contract requirements"
    );

    for requirement in requirements {
        let id = string_field(requirement, "requirement_id");
        let status = string_field(requirement, "status");
        if requirement["level"].as_str() == Some("MUST") {
            assert_eq!(status, "covered", "MUST requirement {id} is not covered");
        }
        for fixture_id in string_array(&requirement["fixture_ids"]) {
            assert!(
                fixture_id == "*" || known_fixture_ids.contains(fixture_id),
                "requirement {id} references unknown fixture {fixture_id}"
            );
        }
    }
}

#[test]
fn generated_fixtures_match_expected_contract_states_and_privacy_invariants() {
    let matrix = fixture_matrix();
    let validator = load_schema_validator();

    for scenario in scenario_array(&matrix) {
        let report = build_scenario_report(scenario);
        let expected = &scenario["expected"];
        assert_eq!(
            enum_json(report.snapshot.overall_state),
            expected["overall_state"],
            "{} overall state drifted",
            report.scenario_id
        );
        assert_eq!(
            enum_json(report.snapshot.evidence_state),
            expected["evidence_state"],
            "{} evidence state drifted",
            report.scenario_id
        );
        assert_eq!(
            enum_json(report.snapshot.pressure_tier),
            expected["pressure_tier"],
            "{} pressure tier drifted",
            report.scenario_id
        );
        assert_eq!(
            report
                .snapshot
                .admission_action
                .map_or_else(|| Value::String("none".to_string()), enum_json),
            expected["admission_action"],
            "{} admission action drifted",
            report.scenario_id
        );
        assert_eq!(
            report
                .snapshot
                .dominant_kind
                .map_or_else(|| Value::String("none".to_string()), enum_json),
            expected["dominant_kind"],
            "{} dominant kind drifted",
            report.scenario_id
        );

        for reason in string_array(&expected["required_reason_codes"]) {
            assert!(
                report
                    .snapshot
                    .reason_codes
                    .iter()
                    .any(|actual| actual == reason),
                "{} missing root reason code {reason}",
                report.scenario_id
            );
        }

        let dry_run_reasons = serde_json::to_string(&report.dry_run_plan)
            .expect("dry-run plan serializes for reason-code assertions");
        for reason in string_array(&expected["required_plan_reason_codes"]) {
            assert!(
                dry_run_reasons.contains(reason),
                "{} missing dry-run reason code {reason}",
                report.scenario_id
            );
        }

        let min_rows = expected["min_calendar_rows"]
            .as_u64()
            .expect("min_calendar_rows is present");
        assert!(
            u64::try_from(report.dry_run_plan.calendar.len()).expect("len fits u64") >= min_rows,
            "{} calendar row count drifted",
            report.scenario_id
        );
        assert!(report.dry_run_plan.dry_run_only);
        assert!(!report.dry_run_plan.live_mutation_allowed);
        assert!(!report.snapshot.raw_pane_content_stored);
        assert_eq!(
            report.projection["target_class_hardware_proof"]["available"].as_bool(),
            Some(false),
            "{} must not imply target-class hardware proof",
            report.scenario_id
        );

        if let Err(errors) = validator.validate(&report.projection) {
            let messages = errors
                .map(|error| format!("{}: {}", error.instance_path, error))
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "{} projection did not match ft-herd-wave schema:\n{messages}",
                report.scenario_id
            );
        }

        let serialized = serde_json::to_string(&report.projection).expect("projection serializes");
        assert!(
            !contains_any_private_sentinel(&serialized),
            "{} leaked a privacy sentinel",
            report.scenario_id
        );
        assert_eq!(
            report.projection["raw_pane_content_stored"].as_bool(),
            Some(false),
            "{} privacy flag drifted",
            report.scenario_id
        );
    }
}

#[test]
fn json_and_toon_goldens_preserve_reason_codes_and_unavailable_sources() {
    let matrix = fixture_matrix();

    for scenario in scenario_array(&matrix) {
        let report = build_scenario_report(scenario);
        let decoded = toon_roundtrip_json(&report.projection);
        assert_eq!(
            decoded["reason_codes"], report.projection["reason_codes"],
            "{} TOON roundtrip changed reason_codes",
            report.scenario_id
        );
        assert_eq!(
            decoded["unavailable_sources"], report.projection["unavailable_sources"],
            "{} TOON roundtrip changed unavailable_sources",
            report.scenario_id
        );
        assert_eq!(
            decoded["forbidden_actions"], report.projection["forbidden_actions"],
            "{} TOON roundtrip changed forbidden_actions",
            report.scenario_id
        );
        assert_eq!(
            decoded["raw_pane_content_stored"].as_bool(),
            Some(false),
            "{} TOON roundtrip changed privacy flag",
            report.scenario_id
        );
    }
}
