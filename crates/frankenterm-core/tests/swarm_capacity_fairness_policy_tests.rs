//! Tests for the deterministic fairness policy table exposed by
//! `SwarmCapacityFairnessPolicy::with_defaults().policy_table()` in
//! `frankenterm_core::runtime_telemetry`. The default WFQ policy carries
//! real fairness invariants (one row per work class, per-mille minimum
//! service bounds, no over-commitment) but had no coverage. These pin the
//! contract without reaching into the admission controller.

use std::collections::HashSet;

use frankenterm_core::runtime_telemetry::{SwarmCapacityFairnessPolicy, SwarmCapacityWorkClass};

#[test]
fn fairness_policy_table_covers_each_work_class_once() {
    let policy = SwarmCapacityFairnessPolicy::with_defaults();
    let table = policy.policy_table();
    assert_eq!(
        table.len(),
        SwarmCapacityWorkClass::ALL.len(),
        "policy table must have one row per work class"
    );
    let classes: HashSet<SwarmCapacityWorkClass> =
        table.iter().map(|row| row.work_class).collect();
    assert_eq!(classes.len(), table.len(), "each work class must appear exactly once");
}

#[test]
fn fairness_policy_minimum_service_is_bounded_and_not_overcommitted() {
    let policy = SwarmCapacityFairnessPolicy::with_defaults();
    let table = policy.policy_table();

    let mut total_min_service: u32 = 0;
    for row in table {
        // Per-mille service target cannot exceed 100% on its own.
        assert!(
            row.minimum_service_per_1000 <= 1000,
            "minimum_service_per_1000 must be <= 1000 for {:?}, got {}",
            row.work_class, row.minimum_service_per_1000
        );
        // Every class must retain at least one eligible pressure action.
        assert!(
            !row.eligible_pressure_actions.is_empty(),
            "each work class must have at least one eligible pressure action ({:?})",
            row.work_class
        );
        total_min_service += u32::from(row.minimum_service_per_1000);
    }

    // The sum of minimum service guarantees must not over-commit the budget:
    // you cannot promise more than 1000 per-mille (100%) of total service.
    assert!(
        total_min_service <= 1000,
        "sum of minimum service guarantees {total_min_service} must not exceed 1000 per-mille"
    );
}

#[test]
fn fairness_policy_table_is_deterministic() {
    let a = SwarmCapacityFairnessPolicy::with_defaults();
    let b = SwarmCapacityFairnessPolicy::with_defaults();
    assert_eq!(
        a.policy_table(),
        b.policy_table(),
        "the default fairness policy table must be deterministic"
    );
}
