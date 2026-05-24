//! Invariant tests for the swarm-capacity resource-budget planner in
//! `frankenterm_core::runtime_telemetry`. Builds arbitrary hardware
//! fingerprints + workload mixes through the public API and drives the real
//! `plan_swarm_capacity_resource_budget` path (stronger than the curated
//! dry-run examples), asserting the budget-accounting + planning invariants.

use proptest::prelude::*;

use frankenterm_core::fleet_memory_controller::FleetPressureTier;
use frankenterm_core::runtime_telemetry::{
    plan_swarm_capacity_resource_budget, SwarmCapacityAgentWorkloadClass,
    SwarmCapacityBudgetWorkloadMixRow, SwarmCapacityHardwareFingerprint,
};

fn arb_class() -> impl Strategy<Value = SwarmCapacityAgentWorkloadClass> {
    prop::sample::select(SwarmCapacityAgentWorkloadClass::ALL.to_vec())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For any hardware + workload mix, the resource-budget plan is a
    /// side-effect-free dry-run that echoes the timestamp, produces
    /// non-empty subsystem budgets where available never exceeds budget,
    /// and whose overall pressure tier is the worst subsystem tier.
    #[test]
    fn resource_budget_plan_accounting_invariants(
        cpus in 1u32..256,
        mem_gib in 1u64..2048,
        mix in prop::collection::vec((arb_class(), 0u32..500), 1..6),
    ) {
        let hardware = SwarmCapacityHardwareFingerprint::new(
            Some(cpus),
            Some(mem_gib * 1024 * 1024 * 1024),
        );
        let workload_mix: Vec<SwarmCapacityBudgetWorkloadMixRow> = mix
            .into_iter()
            .map(|(class, count)| SwarmCapacityBudgetWorkloadMixRow::new(class, count))
            .collect();

        let ts = 1_700_000_000_000;
        let plan =
            plan_swarm_capacity_resource_budget(ts, "test", hardware, &workload_mix);

        // Planning surface: dry-run, side-effect free, timestamp echoed.
        prop_assert!(plan.dry_run);
        prop_assert!(!plan.side_effects_executed);
        prop_assert_eq!(plan.generated_at_ms, ts);

        prop_assert!(!plan.subsystem_budgets.is_empty(),
            "a plan must allocate at least one subsystem budget");

        // Per-subsystem accounting: available is remaining capacity, so it
        // can never exceed the total budget.
        for row in &plan.subsystem_budgets {
            prop_assert!(row.available <= row.budget,
                "{:?}: available {} must not exceed budget {}",
                row.subsystem, row.available, row.budget);
        }

        // The plan's overall pressure tier is the worst subsystem tier.
        let worst = plan
            .subsystem_budgets
            .iter()
            .map(|row| row.pressure_tier)
            .max()
            .unwrap_or(FleetPressureTier::Normal);
        prop_assert_eq!(plan.pressure_tier, worst,
            "plan pressure tier must be the worst subsystem tier");
    }
}
