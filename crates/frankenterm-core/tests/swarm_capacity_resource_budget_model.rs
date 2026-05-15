use std::fs;
use std::path::{Path, PathBuf};

use frankenterm_core::runtime_telemetry::{
    SWARM_CAPACITY_RESOURCE_BUDGET_CONTRACT_ID, SWARM_CAPACITY_RESOURCE_BUDGET_SCHEMA_VERSION,
    SwarmCapacityAgentWorkloadClass, SwarmCapacityBudgetSubsystem,
    SwarmCapacityBudgetWorkloadMixRow, SwarmCapacityHardwareClass,
    SwarmCapacityHardwareFingerprint, SwarmCapacityResourceBudgetPlan,
    SwarmCapacityWorkloadEvidenceState, plan_swarm_capacity_resource_budget,
    swarm_capacity_resource_budget_dry_run_examples,
};
use serde_json::Value;

const GIB: u64 = 1024 * 1024 * 1024;
const FIXTURE_GENERATED_AT_MS: u64 = 1_700_000_000_000;
const TARGET_CLASS_SUMMARY_FALLBACK: &str = r#"{
  "hardware_predicate": {
    "target_class": false,
    "proof_status": "skipped_not_proven"
  },
  "observed_host": {
    "logical_cpus": 14,
    "memory_gib": 64
  }
}"#;

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf()
}

fn target_class_summary_path() -> PathBuf {
    workspace_root()
        .join("tests")
        .join("e2e")
        .join("artifacts")
        .join("target-class")
        .join("linux-x86_64-high-core")
        .join("20260512T150000Z")
        .join("summary.json")
}

fn parse_json_text(path: &Path, text: &str) -> Value {
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("failed to parse JSON {}: {err}", path.display()))
}

fn load_target_class_summary() -> Value {
    let path = target_class_summary_path();
    match fs::read_to_string(&path) {
        Ok(text) => parse_json_text(&path, &text),
        Err(_) => parse_json_text(&path, TARGET_CLASS_SUMMARY_FALLBACK),
    }
}

fn subsystem_budget<'a>(
    plan: &'a SwarmCapacityResourceBudgetPlan,
    subsystem: SwarmCapacityBudgetSubsystem,
) -> &'a frankenterm_core::runtime_telemetry::SwarmCapacitySubsystemBudgetRow {
    plan.subsystem_budgets
        .iter()
        .find(|row| row.subsystem == subsystem)
        .unwrap_or_else(|| panic!("missing subsystem row {subsystem:?}"))
}

#[test]
fn dry_run_examples_cover_all_hardware_classes_and_subsystems() {
    let examples = swarm_capacity_resource_budget_dry_run_examples(FIXTURE_GENERATED_AT_MS);
    let classes = examples
        .iter()
        .map(|plan| plan.hardware.hardware_class)
        .collect::<Vec<_>>();
    assert_eq!(classes, SwarmCapacityHardwareClass::ALL);

    for plan in &examples {
        assert_eq!(
            plan.schema_version,
            SWARM_CAPACITY_RESOURCE_BUDGET_SCHEMA_VERSION
        );
        assert_eq!(plan.contract_id, SWARM_CAPACITY_RESOURCE_BUDGET_CONTRACT_ID);
        assert!(plan.dry_run);
        assert!(!plan.side_effects_executed);
        assert!(!plan.lower_bound);
        let subsystems = plan
            .subsystem_budgets
            .iter()
            .map(|row| row.subsystem)
            .collect::<Vec<_>>();
        assert_eq!(subsystems, SwarmCapacityBudgetSubsystem::ALL);
        assert!(plan.total_agent_count > 0);
        assert!(plan.total_requested_units >= plan.total_agent_count);
    }

    let high_core = examples
        .iter()
        .find(|plan| plan.hardware.hardware_class == SwarmCapacityHardwareClass::HighCore)
        .expect("high-core example");
    assert_eq!(high_core.hardware.logical_cpus, Some(64));
    assert_eq!(high_core.hardware.memory_bytes, Some(256 * GIB));
    assert_eq!(high_core.hardware.class_logical_cpu_floor, 64);
    assert_eq!(high_core.hardware.class_memory_bytes_floor, 256 * GIB);
    assert_eq!(
        subsystem_budget(high_core, SwarmCapacityBudgetSubsystem::BuildSlots).budget,
        12
    );
    assert_eq!(
        subsystem_budget(high_core, SwarmCapacityBudgetSubsystem::RchOffload).budget,
        16
    );
}

#[test]
fn missing_cpu_or_memory_uses_low_lower_bound_budget() {
    let hardware = SwarmCapacityHardwareFingerprint::new(None, Some(256 * GIB));
    let mix = [SwarmCapacityBudgetWorkloadMixRow::new(
        SwarmCapacityAgentWorkloadClass::Building,
        4,
    )];
    let plan = plan_swarm_capacity_resource_budget(FIXTURE_GENERATED_AT_MS, "test", hardware, &mix);

    assert!(plan.lower_bound);
    assert_eq!(
        plan.hardware.hardware_class,
        SwarmCapacityHardwareClass::Low
    );
    assert_eq!(
        plan.hardware.evidence_state,
        SwarmCapacityWorkloadEvidenceState::Unavailable
    );
    assert!(
        plan.reason_codes
            .contains(&"capacity.budget.hardware.lower_bound_missing_cpu".to_string())
    );

    let build = subsystem_budget(&plan, SwarmCapacityBudgetSubsystem::BuildSlots);
    assert_eq!(build.budget, 1);
    assert_eq!(build.used, 4);
    assert_eq!(build.available, 0);
    assert!(
        build
            .reason_codes
            .contains(&"capacity.budget.hardware.lower_bound".to_string())
    );
}

#[test]
fn duplicate_workload_mix_rows_merge_and_counters_saturate() {
    let hardware = SwarmCapacityHardwareFingerprint::new(Some(64), Some(256 * GIB));
    let mix = [
        SwarmCapacityBudgetWorkloadMixRow::new(SwarmCapacityAgentWorkloadClass::Coding, u32::MAX),
        SwarmCapacityBudgetWorkloadMixRow::new(SwarmCapacityAgentWorkloadClass::Coding, u32::MAX),
        SwarmCapacityBudgetWorkloadMixRow::new(SwarmCapacityAgentWorkloadClass::Building, u32::MAX),
    ];
    let plan = plan_swarm_capacity_resource_budget(FIXTURE_GENERATED_AT_MS, "test", hardware, &mix);

    let coding = plan
        .workload_mix
        .iter()
        .find(|row| row.workload_class == SwarmCapacityAgentWorkloadClass::Coding)
        .expect("merged coding row");
    assert_eq!(coding.agent_count, u32::MAX);
    assert_eq!(plan.workload_mix.len(), 2);

    let memory = subsystem_budget(&plan, SwarmCapacityBudgetSubsystem::MemoryTiers);
    assert_eq!(memory.available, 0);
    assert!(memory.saturation_per_1000 >= 1_000);

    let build = subsystem_budget(&plan, SwarmCapacityBudgetSubsystem::BuildSlots);
    assert_eq!(build.used, u64::from(u32::MAX));
    assert_eq!(build.available, 0);
    assert!(build.saturation_per_1000 >= 1_000);
}

#[test]
fn skipped_target_class_artifact_does_not_grant_high_core_budget() {
    let summary = load_target_class_summary();
    assert_eq!(
        summary["hardware_predicate"]["target_class"].as_bool(),
        Some(false)
    );
    assert_eq!(
        summary["hardware_predicate"]["proof_status"].as_str(),
        Some("skipped_not_proven")
    );

    let observed_cpus = summary["observed_host"]["logical_cpus"]
        .as_u64()
        .expect("observed logical cpus") as u32;
    let observed_memory_bytes = summary["observed_host"]["memory_gib"]
        .as_u64()
        .expect("observed memory gib")
        .saturating_mul(GIB);
    let hardware =
        SwarmCapacityHardwareFingerprint::new(Some(observed_cpus), Some(observed_memory_bytes));
    let plan = plan_swarm_capacity_resource_budget(
        FIXTURE_GENERATED_AT_MS,
        "target_class.summary",
        hardware,
        &[SwarmCapacityBudgetWorkloadMixRow::new(
            SwarmCapacityAgentWorkloadClass::Building,
            8,
        )],
    );

    assert_ne!(
        plan.hardware.hardware_class,
        SwarmCapacityHardwareClass::HighCore
    );
    assert!(!plan.lower_bound);
    assert!(
        plan.reason_codes
            .iter()
            .any(|reason| reason == "capacity.budget.hardware.class.mid")
    );
    let examples = swarm_capacity_resource_budget_dry_run_examples(FIXTURE_GENERATED_AT_MS);
    let high_core = examples
        .iter()
        .find(|candidate| candidate.hardware.hardware_class == SwarmCapacityHardwareClass::HighCore)
        .expect("high-core example");
    assert!(
        subsystem_budget(&plan, SwarmCapacityBudgetSubsystem::BuildSlots).budget
            < subsystem_budget(high_core, SwarmCapacityBudgetSubsystem::BuildSlots).budget
    );
}

#[test]
fn resource_budget_plan_round_trips_through_json_and_toon() {
    let plan = swarm_capacity_resource_budget_dry_run_examples(FIXTURE_GENERATED_AT_MS)
        .into_iter()
        .find(|plan| plan.hardware.hardware_class == SwarmCapacityHardwareClass::HighCore)
        .expect("high-core example");
    let json_value = serde_json::to_value(&plan).expect("plan serializes");

    let toon = toon_rust::encode(json_value.clone(), None);
    let decoded = toon_rust::try_decode(&toon, None).expect("decode resource budget toon");
    let json = toon_rust::cli::json_stringify::json_stringify_lines(&decoded, 0).join("\n");
    let roundtripped: Value = serde_json::from_str(&json).expect("toon decoded json");

    assert_eq!(
        roundtripped["schema_version"].as_f64(),
        Some(f64::from(SWARM_CAPACITY_RESOURCE_BUDGET_SCHEMA_VERSION))
    );
    assert_eq!(roundtripped["contract_id"], json_value["contract_id"]);
    assert_eq!(
        roundtripped["hardware"]["hardware_class"],
        json_value["hardware"]["hardware_class"]
    );
    assert_eq!(
        roundtripped["workload_mix"]
            .as_array()
            .expect("roundtripped workload mix")
            .len(),
        json_value["workload_mix"]
            .as_array()
            .expect("json workload mix")
            .len()
    );
    assert_eq!(
        roundtripped["subsystem_budgets"]
            .as_array()
            .expect("roundtripped subsystem budgets")
            .len(),
        json_value["subsystem_budgets"]
            .as_array()
            .expect("json subsystem budgets")
            .len()
    );
    assert_eq!(roundtripped["pressure_tier"], json_value["pressure_tier"]);
    assert_eq!(roundtripped["lower_bound"], json_value["lower_bound"]);
    assert_eq!(
        roundtripped["side_effects_executed"],
        json_value["side_effects_executed"]
    );
}
