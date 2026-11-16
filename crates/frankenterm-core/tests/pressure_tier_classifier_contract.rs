// =============================================================================
// Contract tests for the fleet-memory pressure-fusion classifier
// (`FleetMemoryTierBudgetSnapshot::pressure_tier`).
//
// The inline suite covers the per-source tier maps (map_backpressure /
// map_memory_pressure / map_budget_level), the as_u8 severity ordering, and
// per-tier *action* behavior — but not the snapshot classifier's own threshold
// transitions (Normal/Elevated/Critical/Emergency) or its severity monotonicity
// in over-budget bytes. Those are pinned here.
//
// Classifier contract (fleet_memory_controller.rs:315):
//   refused>0 && resident-over-budget>0     -> Emergency
//   refused>0                               -> Critical
//   resident-over-budget == 0               -> Normal
//   resident-budget == 0 (but over-budget)  -> Emergency (div-by-zero guard)
//   over% = over*10000/budget: <=500 Elevated, <=2500 Critical, else Emergency
//
// All synchronous + default-feature: proves under `cargo test -p frankenterm-core`.
// =============================================================================

use frankenterm_core::fleet_memory_controller::{
    FleetMemoryTier, FleetMemoryTierBudgetRecord, FleetMemoryTierBudgetSnapshot, FleetPressureTier,
};

/// Single resident-tier (HotResident) snapshot with the given budget/actual.
fn hot(budget: u64, actual: u64) -> FleetMemoryTierBudgetSnapshot {
    FleetMemoryTierBudgetSnapshot::from_tiers([FleetMemoryTierBudgetRecord::new(
        FleetMemoryTier::HotResident,
        budget,
        actual,
    )])
}

/// Single resident-tier snapshot with a refused-admission counter.
fn hot_refused(budget: u64, actual: u64, refused: u64) -> FleetMemoryTierBudgetSnapshot {
    FleetMemoryTierBudgetSnapshot::from_tiers([FleetMemoryTierBudgetRecord::new(
        FleetMemoryTier::HotResident,
        budget,
        actual,
    )
    .with_counters(0, 0, refused)])
}

#[test]
fn pressure_tier_normal_when_within_budget() {
    assert_eq!(hot(1_000, 500).pressure_tier(), FleetPressureTier::Normal);
    assert_eq!(hot(1_000, 1_000).pressure_tier(), FleetPressureTier::Normal);
}

#[test]
fn pressure_tier_elevated_up_to_5pct_overage() {
    assert_eq!(
        hot(1_000, 1_040).pressure_tier(),
        FleetPressureTier::Elevated
    );
    // Exactly 5% overage (over% == 500) is the inclusive upper edge of Elevated.
    assert_eq!(
        hot(1_000, 1_050).pressure_tier(),
        FleetPressureTier::Elevated
    );
}

#[test]
fn pressure_tier_critical_between_5pct_and_25pct() {
    assert_eq!(
        hot(1_000, 1_100).pressure_tier(),
        FleetPressureTier::Critical
    );
    // Exactly 25% overage (over% == 2500) is the inclusive upper edge of Critical.
    assert_eq!(
        hot(1_000, 1_250).pressure_tier(),
        FleetPressureTier::Critical
    );
}

#[test]
fn pressure_tier_emergency_above_25pct() {
    // over% = 2501 (> 2500) — first step past the Critical ceiling.
    assert_eq!(
        hot(10_000, 12_501).pressure_tier(),
        FleetPressureTier::Emergency
    );
    assert_eq!(
        hot(1_000, 2_000).pressure_tier(),
        FleetPressureTier::Emergency
    );
}

#[test]
fn pressure_tier_refused_without_overage_is_critical() {
    // refused admissions with no resident overage escalates to Critical.
    assert_eq!(
        hot_refused(1_000, 1_000, 50).pressure_tier(),
        FleetPressureTier::Critical
    );
}

#[test]
fn pressure_tier_refused_with_overage_is_emergency() {
    // Both refusing AND over budget is the worst case.
    assert_eq!(
        hot_refused(1_000, 1_100, 50).pressure_tier(),
        FleetPressureTier::Emergency
    );
}

#[test]
fn pressure_tier_zero_budget_with_overage_is_emergency() {
    // Div-by-zero guard: zero resident budget but nonzero usage is Emergency,
    // never a panic.
    assert_eq!(hot(0, 100).pressure_tier(), FleetPressureTier::Emergency);
}

/// Severity monotonicity: for a fixed budget, the classified tier is
/// non-decreasing as observed (over-budget) bytes grow. FleetPressureTier
/// derives Ord with variants in severity order, so `>=` is the severity test.
#[test]
fn pressure_tier_monotone_nondecreasing_in_overage() {
    let budget = 1_000u64;
    let actuals = [500u64, 1_000, 1_050, 1_100, 1_250, 1_300, 2_000, 10_000];
    let mut prev = FleetPressureTier::Normal;
    for actual in actuals {
        let tier = hot(budget, actual).pressure_tier();
        assert!(
            tier >= prev,
            "pressure tier must not decrease as overage grows: actual={actual} gave {tier:?} < {prev:?}"
        );
        prev = tier;
    }
    // The largest overage saturates at the most severe tier.
    assert_eq!(prev, FleetPressureTier::Emergency);
}
