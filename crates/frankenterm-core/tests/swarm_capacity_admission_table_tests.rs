//! Tests for the public `swarm_capacity_workload_admission_table()` reference
//! table in `frankenterm_core::runtime_telemetry`. The table is deterministic
//! (no inputs) and encodes a fail-closed admission contract per workload
//! class, but had no coverage. These pin its structural + fail-closed
//! invariants without reaching into the operator-owned admission controller.

use std::collections::HashSet;

use frankenterm_core::runtime_telemetry::{
    swarm_capacity_workload_admission_dry_run_examples, swarm_capacity_workload_admission_table,
    SwarmCapacityAdmissionAction, SwarmCapacityAgentWorkloadClass,
};

#[test]
fn workload_admission_table_covers_each_class_exactly_once() {
    let table = swarm_capacity_workload_admission_table();
    assert_eq!(
        table.len(),
        SwarmCapacityAgentWorkloadClass::ALL.len(),
        "table must have one row per workload class"
    );
    let classes: HashSet<SwarmCapacityAgentWorkloadClass> =
        table.iter().map(|row| row.workload_class).collect();
    assert_eq!(classes.len(), table.len(), "each workload class must appear exactly once");
}

#[test]
fn workload_admission_table_fails_closed_on_degraded_evidence() {
    for row in swarm_capacity_workload_admission_table() {
        // Stale and unavailable evidence are treated identically (both are
        // baseline.max_conservative(Defer)).
        assert_eq!(
            row.stale_evidence_action, row.unavailable_evidence_action,
            "stale/unavailable evidence must yield the same action for {:?}",
            row.workload_class
        );
        // max_conservative(_, Defer) is at least as conservative as Defer, so a
        // degraded-evidence row can never fall back to a normal Admit.
        assert_ne!(
            row.stale_evidence_action,
            SwarmCapacityAdmissionAction::Admit,
            "degraded evidence must never admit normally for {:?}",
            row.workload_class
        );
        assert_ne!(
            row.unavailable_evidence_action,
            SwarmCapacityAdmissionAction::Admit,
            "unavailable evidence must never admit normally for {:?}",
            row.workload_class
        );
    }
}

#[test]
fn workload_admission_table_reason_codes_are_well_formed() {
    for row in swarm_capacity_workload_admission_table() {
        assert_eq!(
            row.reason_codes,
            vec![format!("capacity.workload.class.{}", row.workload_class.as_str())],
            "reason code must follow capacity.workload.class.<name> for {:?}",
            row.workload_class
        );
    }
}

#[test]
fn workload_admission_table_is_deterministic() {
    assert_eq!(
        swarm_capacity_workload_admission_table(),
        swarm_capacity_workload_admission_table(),
        "the reference table must be deterministic"
    );
}

#[test]
fn workload_admission_dry_run_plan_is_safe_and_private() {
    for ts in [0_u64, 1, 1_700_000_000_000, u64::MAX] {
        let plan = swarm_capacity_workload_admission_dry_run_examples(ts);

        // Planning surface: dry-run only, no side effects, no raw pane content.
        assert!(plan.dry_run, "dry-run example plan must set dry_run");
        assert!(!plan.side_effects_executed, "planning must never execute side effects");
        assert!(!plan.raw_pane_content_stored, "planning must never store raw pane content");
        // The supplied timestamp is echoed unchanged.
        assert_eq!(plan.generated_at_ms, ts, "generated_at_ms must echo the input");
        assert!(!plan.decisions.is_empty(), "dry-run examples must produce decisions");

        for decision in &plan.decisions {
            assert!(
                !decision.side_effects_executed,
                "each dry-run decision must not execute side effects (id={})",
                decision.stable_id
            );
            assert!(
                decision.admitted_units <= decision.requested_units,
                "admitted_units {} must not exceed requested_units {} (id={})",
                decision.admitted_units, decision.requested_units, decision.stable_id
            );
        }
    }
}

#[test]
fn workload_admission_dry_run_plan_is_deterministic() {
    assert_eq!(
        swarm_capacity_workload_admission_dry_run_examples(1_700_000_000_000),
        swarm_capacity_workload_admission_dry_run_examples(1_700_000_000_000),
        "dry-run example plan must be deterministic for a fixed timestamp"
    );
}
