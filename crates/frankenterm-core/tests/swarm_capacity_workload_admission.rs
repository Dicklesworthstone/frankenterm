//! Conformance tests for the ft-b94bx.2 workload-class admission model.

use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::runtime_telemetry::{
    HealthTier, SWARM_CAPACITY_WORKLOAD_ADMISSION_CONTRACT_ID,
    SWARM_CAPACITY_WORKLOAD_ADMISSION_SCHEMA_VERSION, SwarmCapacityAdmissionAction,
    SwarmCapacityAgentWorkloadClass, SwarmCapacityTelemetryGapState,
    SwarmCapacityWorkloadAdmissionInput, SwarmCapacityWorkloadAdmissionSignal,
    SwarmCapacityWorkloadAdmissionSignals, SwarmCapacityWorkloadEvidenceState,
    SwarmCapacityWorkloadSignalKind, plan_swarm_capacity_workload_admission,
    swarm_capacity_workload_admission_dry_run_examples, swarm_capacity_workload_admission_table,
};
use proptest::prelude::*;
use serde_json::{Value, json};

const FIXTURE_GENERATED_AT_MS: u64 = 1_700_000_000_000;

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("swarm_capacity_workload_admission")
        .join("examples.json")
}

fn doc_path() -> PathBuf {
    workspace_root()
        .join("docs")
        .join("swarm-capacity-workload-admission.md")
}

fn load_json(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse JSON {}: {err}", path.display()))
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

fn arb_workload_class() -> impl Strategy<Value = SwarmCapacityAgentWorkloadClass> {
    prop_oneof![
        Just(SwarmCapacityAgentWorkloadClass::Coding),
        Just(SwarmCapacityAgentWorkloadClass::Reviewing),
        Just(SwarmCapacityAgentWorkloadClass::Building),
        Just(SwarmCapacityAgentWorkloadClass::Testing),
        Just(SwarmCapacityAgentWorkloadClass::Idle),
        Just(SwarmCapacityAgentWorkloadClass::Blocked),
        Just(SwarmCapacityAgentWorkloadClass::RateLimited),
        Just(SwarmCapacityAgentWorkloadClass::ContextSaturated),
        Just(SwarmCapacityAgentWorkloadClass::StuckTuiHeavy),
    ]
}

fn arb_degraded_evidence_state() -> impl Strategy<Value = SwarmCapacityWorkloadEvidenceState> {
    prop_oneof![
        Just(SwarmCapacityWorkloadEvidenceState::Stale),
        Just(SwarmCapacityWorkloadEvidenceState::Redacted),
        Just(SwarmCapacityWorkloadEvidenceState::Contradictory),
        Just(SwarmCapacityWorkloadEvidenceState::Unavailable),
    ]
}

fn plan_one(
    class: SwarmCapacityAgentWorkloadClass,
    signals: SwarmCapacityWorkloadAdmissionSignals,
) -> SwarmCapacityAdmissionAction {
    let mut input = SwarmCapacityWorkloadAdmissionInput::new("prop", 200, class);
    input.signals = signals;
    let plan = plan_swarm_capacity_workload_admission(42, "test", &[input]);
    plan.decisions.first().expect("one decision").action
}

#[test]
fn admission_table_covers_every_required_workload_class() {
    let table = swarm_capacity_workload_admission_table();
    assert_eq!(table.len(), SwarmCapacityAgentWorkloadClass::ALL.len());

    for (row, class) in table.iter().zip(SwarmCapacityAgentWorkloadClass::ALL) {
        assert_eq!(row.workload_class, class);
        assert_eq!(row.measured_green_action, class.baseline_admission_action());
        assert_eq!(
            row.stale_evidence_action,
            SwarmCapacityAdmissionAction::Defer
        );
        assert_eq!(row.unavailable_evidence_action, row.stale_evidence_action);
        assert_eq!(row.requested_units, class.default_requested_units());
        assert!(
            row.reason_codes
                .iter()
                .any(|code| code == &format!("capacity.workload.class.{}", class.as_str()))
        );
    }
}

#[test]
fn every_class_transitions_conservatively_for_stale_and_unavailable_evidence() {
    for class in SwarmCapacityAgentWorkloadClass::ALL {
        let measured_action = plan_one(
            class,
            SwarmCapacityWorkloadAdmissionSignals::measured_green(),
        );
        assert_eq!(measured_action, class.baseline_admission_action());

        for degraded in [
            SwarmCapacityWorkloadEvidenceState::Stale,
            SwarmCapacityWorkloadEvidenceState::Unavailable,
        ] {
            let mut signals = SwarmCapacityWorkloadAdmissionSignals::measured_green();
            signals.context_horizon = SwarmCapacityWorkloadAdmissionSignal::new(
                SwarmCapacityWorkloadSignalKind::ContextHorizon,
                degraded,
                HealthTier::Green,
            );
            let action = plan_one(class, signals);
            assert!(
                action.conservatism_rank() >= measured_action.conservatism_rank(),
                "{class:?} degraded evidence must not upgrade {measured_action:?} to {action:?}"
            );
            assert!(
                action.conservatism_rank()
                    >= SwarmCapacityAdmissionAction::Defer.conservatism_rank(),
                "{class:?} degraded evidence must fail closed to at least defer"
            );
        }
    }
}

#[test]
fn signal_pressure_table_composes_context_blocker_herd_and_resource_gates() {
    let mut context_red = SwarmCapacityWorkloadAdmissionSignals::measured_green();
    context_red.context_horizon = SwarmCapacityWorkloadAdmissionSignal::new(
        SwarmCapacityWorkloadSignalKind::ContextHorizon,
        SwarmCapacityWorkloadEvidenceState::Measured,
        HealthTier::Red,
    );
    assert_eq!(
        plan_one(SwarmCapacityAgentWorkloadClass::Coding, context_red),
        SwarmCapacityAdmissionAction::Defer
    );

    let mut blocker_yellow = SwarmCapacityWorkloadAdmissionSignals::measured_green();
    blocker_yellow.blocker_radar = SwarmCapacityWorkloadAdmissionSignal::new(
        SwarmCapacityWorkloadSignalKind::BlockerRadar,
        SwarmCapacityWorkloadEvidenceState::Measured,
        HealthTier::Yellow,
    );
    assert_eq!(
        plan_one(SwarmCapacityAgentWorkloadClass::Reviewing, blocker_yellow),
        SwarmCapacityAdmissionAction::Defer
    );

    let mut herd_red = SwarmCapacityWorkloadAdmissionSignals::measured_green();
    herd_red.herd_wave = SwarmCapacityWorkloadAdmissionSignal::new(
        SwarmCapacityWorkloadSignalKind::HerdWave,
        SwarmCapacityWorkloadEvidenceState::Measured,
        HealthTier::Red,
    );
    let mut herd_input = SwarmCapacityWorkloadAdmissionInput::new(
        "herd",
        500,
        SwarmCapacityAgentWorkloadClass::Coding,
    );
    herd_input.signals = herd_red;
    let herd_plan = plan_swarm_capacity_workload_admission(42, "test", &[herd_input]);
    let herd_decision = herd_plan.decisions.first().expect("herd decision");
    assert_eq!(herd_decision.action, SwarmCapacityAdmissionAction::Defer);
    assert_eq!(herd_decision.recommended_stagger_ms, Some(3_250));

    let mut resource_red = SwarmCapacityWorkloadAdmissionSignals::measured_green();
    resource_red.resource_pressure = SwarmCapacityWorkloadAdmissionSignal::new(
        SwarmCapacityWorkloadSignalKind::ResourcePressure,
        SwarmCapacityWorkloadEvidenceState::Measured,
        HealthTier::Red,
    );
    assert_eq!(
        plan_one(
            SwarmCapacityAgentWorkloadClass::Building,
            resource_red.clone()
        ),
        SwarmCapacityAdmissionAction::Defer
    );
    assert_eq!(
        plan_one(SwarmCapacityAgentWorkloadClass::StuckTuiHeavy, resource_red),
        SwarmCapacityAdmissionAction::ThrottleCapturePolling
    );

    let mut resource_black = SwarmCapacityWorkloadAdmissionSignals::measured_green();
    resource_black.resource_pressure = SwarmCapacityWorkloadAdmissionSignal::new(
        SwarmCapacityWorkloadSignalKind::ResourcePressure,
        SwarmCapacityWorkloadEvidenceState::Measured,
        HealthTier::Black,
    );
    assert_eq!(
        plan_one(SwarmCapacityAgentWorkloadClass::Idle, resource_black),
        SwarmCapacityAdmissionAction::Shed
    );
}

#[test]
fn invalid_evidence_state_json_is_rejected() {
    let invalid = json!({
        "kind": "context_horizon",
        "evidence_state": "fresh",
        "pressure_tier": "green",
        "reason_codes": ["capacity.workload.context_horizon.fresh"]
    });
    let err = serde_json::from_value::<SwarmCapacityWorkloadAdmissionSignal>(invalid)
        .expect_err("unknown evidence state must be rejected");
    assert!(
        err.to_string().contains("unknown variant") || err.to_string().contains("expected one of"),
        "unexpected serde error: {err}"
    );
}

#[test]
fn fixture_and_doc_cover_required_classes_signals_and_examples() {
    let fixture = load_json(&fixture_path());
    assert_eq!(
        fixture["schema_version"].as_u64(),
        Some(u64::from(SWARM_CAPACITY_WORKLOAD_ADMISSION_SCHEMA_VERSION))
    );
    assert_eq!(
        fixture["contract_id"].as_str(),
        Some(SWARM_CAPACITY_WORKLOAD_ADMISSION_CONTRACT_ID)
    );
    assert_eq!(fixture["dry_run"], Value::Bool(true));
    assert_eq!(fixture["raw_pane_content_stored"], Value::Bool(false));
    assert_eq!(fixture["side_effects_executed"], Value::Bool(false));

    let doc = fs::read_to_string(doc_path()).expect("workload admission doc should be readable");
    for class in SwarmCapacityAgentWorkloadClass::ALL {
        let label = class.as_str();
        assert!(
            fixture["required_workload_classes"]
                .as_array()
                .expect("required classes")
                .iter()
                .any(|value| value.as_str() == Some(label))
        );
        assert!(doc.contains(label), "doc omits workload class {label}");
    }
    for signal in SwarmCapacityWorkloadSignalKind::ALL {
        let label = signal.as_str();
        assert!(
            fixture["required_signal_kinds"]
                .as_array()
                .expect("required signals")
                .iter()
                .any(|value| value.as_str() == Some(label))
        );
        assert!(doc.contains(label), "doc omits signal kind {label}");
    }
    for state in SwarmCapacityTelemetryGapState::ALL {
        let label = state.as_str();
        assert!(
            fixture["telemetry_gap_states"]
                .as_array()
                .expect("telemetry gap states")
                .iter()
                .any(|value| value.as_str() == Some(label)),
            "fixture omits telemetry gap state {label}"
        );
        assert!(doc.contains(label), "doc omits telemetry gap state {label}");
    }
    for reason_code in fixture["fail_closed_reason_codes"]
        .as_array()
        .expect("fail closed reason code catalog")
        .iter()
        .filter_map(Value::as_str)
    {
        assert!(
            doc.contains(reason_code),
            "doc omits fail-closed reason code {reason_code}"
        );
    }
}

#[test]
fn dry_run_examples_match_fixture_and_never_execute_side_effects() {
    let fixture = load_json(&fixture_path());
    let plan = swarm_capacity_workload_admission_dry_run_examples(FIXTURE_GENERATED_AT_MS);
    let plan_value = serde_json::to_value(&plan).expect("plan serializes");

    assert_eq!(
        plan.schema_version,
        SWARM_CAPACITY_WORKLOAD_ADMISSION_SCHEMA_VERSION
    );
    assert_eq!(
        plan.contract_id,
        SWARM_CAPACITY_WORKLOAD_ADMISSION_CONTRACT_ID
    );
    assert!(plan.dry_run);
    assert!(!plan.raw_pane_content_stored);
    assert!(!plan.side_effects_executed);
    assert_eq!(
        plan.admission_table.len(),
        SwarmCapacityAgentWorkloadClass::ALL.len()
    );

    for expected in fixture["expected_decisions"]
        .as_array()
        .expect("expected decisions")
    {
        let stable_id = expected["stable_id"].as_str().expect("stable_id string");
        let actual = plan_value["decisions"]
            .as_array()
            .expect("plan decisions")
            .iter()
            .find(|decision| decision["stable_id"] == stable_id)
            .unwrap_or_else(|| panic!("missing decision {stable_id}"));
        for key in [
            "pane_scale",
            "workload_class",
            "work_class",
            "action",
            "evidence_state",
            "requested_units",
            "admitted_units",
        ] {
            assert_eq!(
                actual[key], expected[key],
                "decision {stable_id} field {key}"
            );
        }
        let expected_stagger = &expected["recommended_stagger_ms"];
        if expected_stagger.is_null() {
            assert!(
                actual.get("recommended_stagger_ms").is_none()
                    || actual["recommended_stagger_ms"].is_null(),
                "decision {stable_id} should not carry a stagger hint"
            );
        } else {
            assert_eq!(
                actual["recommended_stagger_ms"], *expected_stagger,
                "decision {stable_id} stagger hint"
            );
        }
        assert_eq!(actual["side_effects_executed"], Value::Bool(false));
    }
}

#[test]
fn decision_dto_round_trips_through_json_and_toon() {
    let plan = swarm_capacity_workload_admission_dry_run_examples(FIXTURE_GENERATED_AT_MS);
    let decision = plan
        .decisions
        .iter()
        .find(|decision| decision.stable_id == "example.500.context")
        .expect("context example decision");
    let json_value = serde_json::to_value(decision).expect("decision serializes");

    let toon = toon_rust::encode(json_value.clone(), None);
    let decoded = toon_rust::try_decode(&toon, None).expect("decode workload decision toon");
    let json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    let mut roundtripped: Value = serde_json::from_str(&json).expect("toon decoded json");
    normalize_integral_toon_numbers(&mut roundtripped);

    for key in [
        "stable_id",
        "pane_scale",
        "workload_class",
        "work_class",
        "action",
        "evidence_state",
        "telemetry_gap_state",
        "pause_admission",
        "kill_switch_active",
        "recommended_stagger_ms",
        "requested_units",
        "admitted_units",
        "side_effects_executed",
    ] {
        assert_eq!(roundtripped[key], json_value[key], "TOON parity key {key}");
    }
}

proptest! {
    #[test]
    fn degraded_evidence_never_upgrades_admission(
        class in arb_workload_class(),
        degraded in arb_degraded_evidence_state(),
    ) {
        let measured = plan_one(class, SwarmCapacityWorkloadAdmissionSignals::measured_green());
        let mut signals = SwarmCapacityWorkloadAdmissionSignals::measured_green();
        signals.resource_pressure = SwarmCapacityWorkloadAdmissionSignal::new(
            SwarmCapacityWorkloadSignalKind::ResourcePressure,
            degraded,
            HealthTier::Green,
        );
        let degraded_action = plan_one(class, signals);

        prop_assert!(
            degraded_action.conservatism_rank() >= measured.conservatism_rank(),
            "{class:?}: {degraded:?} upgraded {measured:?} to {degraded_action:?}"
        );
        prop_assert!(
            degraded_action.conservatism_rank()
                >= SwarmCapacityAdmissionAction::Defer.conservatism_rank(),
            "{class:?}: {degraded:?} did not fail closed to defer-or-worse"
        );
    }
}
