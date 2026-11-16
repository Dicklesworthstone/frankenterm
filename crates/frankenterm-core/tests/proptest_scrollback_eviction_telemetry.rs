//! Property-based tests for scrollback eviction telemetry counters (ft-3kxe.12).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. Snapshot serde roundtrip
//! 3. plans_computed increments on every plan() call
//! 4. panes_evaluated tracks total panes across plans
//! 5. targets_generated and segments_planned track eviction work
//! 6. executions_run and segments_removed track actual deletions
//! 7. execution_errors counts store failures
//! 8. Counter monotonicity across repeated operations

use proptest::prelude::*;

use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::pane_tiers::PaneTier;
use frankenterm_core::scrollback_eviction::{
    EvictionConfig, PaneTierSource, ScrollbackEvictionTelemetrySnapshot, ScrollbackEvictor,
    SegmentStore,
};

// =============================================================================
// Mock implementations
// =============================================================================

/// In-memory segment store for testing.
struct MockStore {
    panes: Vec<(u64, usize)>,
    fail_on_delete: bool,
}

impl MockStore {
    fn new(panes: Vec<(u64, usize)>) -> Self {
        Self {
            panes,
            fail_on_delete: false,
        }
    }

    fn with_failures(panes: Vec<(u64, usize)>) -> Self {
        Self {
            panes,
            fail_on_delete: true,
        }
    }
}

impl SegmentStore for MockStore {
    fn count_segments(&self, pane_id: u64) -> Result<usize, String> {
        Ok(self
            .panes
            .iter()
            .find(|(id, _)| *id == pane_id)
            .map(|(_, count)| *count)
            .unwrap_or(0))
    }

    fn delete_oldest_segments(&self, _pane_id: u64, count: usize) -> Result<usize, String> {
        if self.fail_on_delete {
            Err("mock store failure".to_string())
        } else {
            Ok(count)
        }
    }

    fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
        Ok(self.panes.iter().map(|(id, _)| *id).collect())
    }
}

/// Simple tier source that assigns all panes as Dormant.
struct AllDormantTierSource;

impl PaneTierSource for AllDormantTierSource {
    fn tier_for(&self, _pane_id: u64) -> Option<PaneTier> {
        Some(PaneTier::Dormant)
    }
}

/// Tier source that maps pane IDs to tiers based on id % 5.
struct CyclingTierSource;

impl PaneTierSource for CyclingTierSource {
    fn tier_for(&self, pane_id: u64) -> Option<PaneTier> {
        Some(match pane_id % 5 {
            0 => PaneTier::Active,
            1 => PaneTier::Thinking,
            2 => PaneTier::Idle,
            3 => PaneTier::Background,
            _ => PaneTier::Dormant,
        })
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let store = MockStore::new(vec![]);
    let evictor = ScrollbackEvictor::new(EvictionConfig::default(), store, AllDormantTierSource);
    let snap = evictor.telemetry().snapshot();

    assert_eq!(snap.plans_computed, 0);
    assert_eq!(snap.executions_run, 0);
    assert_eq!(snap.panes_evaluated, 0);
    assert_eq!(snap.targets_generated, 0);
    assert_eq!(snap.segments_planned, 0);
    assert_eq!(snap.segments_removed, 0);
    assert_eq!(snap.execution_errors, 0);
}

#[test]
fn plan_increments_counters() {
    let store = MockStore::new(vec![(1, 500), (2, 50), (3, 200)]);
    let evictor = ScrollbackEvictor::new(EvictionConfig::default(), store, AllDormantTierSource);

    // Default dormant_max_segments = 100, min_segments = 10
    // Pane 1 (500 > 100) → target, remove 400
    // Pane 2 (50 < 100) → no target
    // Pane 3 (200 > 100) → target, remove 100
    let plan = evictor.plan(MemoryPressureTier::Green).unwrap();
    assert_eq!(plan.panes_affected, 2);

    let snap = evictor.telemetry().snapshot();
    assert_eq!(snap.plans_computed, 1);
    assert_eq!(snap.panes_evaluated, 3);
    assert_eq!(snap.targets_generated, 2);
    assert_eq!(snap.segments_planned, 500);
}

#[test]
fn execute_increments_counters() {
    let store = MockStore::new(vec![(1, 500), (2, 200)]);
    let evictor = ScrollbackEvictor::new(EvictionConfig::default(), store, AllDormantTierSource);

    let plan = evictor.plan(MemoryPressureTier::Green).unwrap();
    let report = evictor.execute(&plan);

    let snap = evictor.telemetry().snapshot();
    assert_eq!(snap.executions_run, 1);
    assert_eq!(snap.segments_removed, report.segments_removed as u64);
    assert_eq!(snap.execution_errors, 0);
}

#[test]
fn execution_errors_tracked() {
    let store = MockStore::with_failures(vec![(1, 500), (2, 200)]);
    let evictor = ScrollbackEvictor::new(EvictionConfig::default(), store, AllDormantTierSource);

    let plan = evictor.plan(MemoryPressureTier::Green).unwrap();
    let report = evictor.execute(&plan);

    assert_eq!(report.segments_removed, 0);
    assert_eq!(report.errors.len(), 2);

    let snap = evictor.telemetry().snapshot();
    assert_eq!(snap.execution_errors, 2);
    assert_eq!(snap.segments_removed, 0);
}

#[test]
fn evict_combines_plan_and_execute() {
    let store = MockStore::new(vec![(1, 300)]);
    let evictor = ScrollbackEvictor::new(EvictionConfig::default(), store, AllDormantTierSource);

    let _report = evictor.evict(MemoryPressureTier::Green).unwrap();
    let snap = evictor.telemetry().snapshot();

    assert_eq!(snap.plans_computed, 1);
    assert_eq!(snap.executions_run, 1);
    assert_eq!(snap.panes_evaluated, 1);
}

#[test]
fn empty_store_still_counts_plan() {
    let store = MockStore::new(vec![]);
    let evictor = ScrollbackEvictor::new(EvictionConfig::default(), store, AllDormantTierSource);

    let plan = evictor.plan(MemoryPressureTier::Green).unwrap();
    assert!(plan.is_empty());

    let snap = evictor.telemetry().snapshot();
    assert_eq!(snap.plans_computed, 1);
    assert_eq!(snap.panes_evaluated, 0);
    assert_eq!(snap.targets_generated, 0);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = ScrollbackEvictionTelemetrySnapshot {
        plans_computed: 10,
        executions_run: 8,
        panes_evaluated: 100,
        targets_generated: 25,
        segments_planned: 5000,
        segments_removed: 4800,
        execution_errors: 3,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: ScrollbackEvictionTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

fn arb_pressure() -> impl Strategy<Value = MemoryPressureTier> {
    prop_oneof![
        Just(MemoryPressureTier::Green),
        Just(MemoryPressureTier::Yellow),
        Just(MemoryPressureTier::Orange),
        Just(MemoryPressureTier::Red),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn plans_computed_equals_call_count(
        plan_count in 1usize..10,
        pressure in arb_pressure(),
    ) {
        let store = MockStore::new(vec![(1, 500), (2, 200)]);
        let evictor = ScrollbackEvictor::new(
            EvictionConfig::default(),
            store,
            AllDormantTierSource,
        );

        for _ in 0..plan_count {
            let _ = evictor.plan(pressure);
        }

        let snap = evictor.telemetry().snapshot();
        prop_assert_eq!(snap.plans_computed, plan_count as u64);
    }

    #[test]
    fn panes_evaluated_scales_with_store_size(
        pane_count in 1usize..20,
        plan_count in 1usize..5,
    ) {
        let panes: Vec<(u64, usize)> = (0..pane_count as u64)
            .map(|id| (id, 500))
            .collect();
        let store = MockStore::new(panes);
        let evictor = ScrollbackEvictor::new(
            EvictionConfig::default(),
            store,
            AllDormantTierSource,
        );

        for _ in 0..plan_count {
            let _ = evictor.plan(MemoryPressureTier::Green);
        }

        let snap = evictor.telemetry().snapshot();
        prop_assert_eq!(
            snap.panes_evaluated,
            (pane_count * plan_count) as u64,
            "panes_evaluated should be pane_count * plan_count"
        );
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(
            prop_oneof![
                Just("plan"),
                Just("evict"),
            ],
            1..15,
        ),
    ) {
        let store = MockStore::new(vec![(1, 500), (2, 300)]);
        let evictor = ScrollbackEvictor::new(
            EvictionConfig::default(),
            store,
            AllDormantTierSource,
        );

        let mut prev_plans = 0u64;
        let mut prev_execs = 0u64;
        let mut prev_removed = 0u64;

        for op in &ops {
            match *op {
                "plan" => { let _ = evictor.plan(MemoryPressureTier::Green); }
                "evict" => { let _ = evictor.evict(MemoryPressureTier::Green); }
                _ => unreachable!(),
            }

            let snap = evictor.telemetry().snapshot();
            prop_assert!(
                snap.plans_computed >= prev_plans,
                "plans_computed must not decrease"
            );
            prop_assert!(
                snap.executions_run >= prev_execs,
                "executions_run must not decrease"
            );
            prop_assert!(
                snap.segments_removed >= prev_removed,
                "segments_removed must not decrease"
            );

            prev_plans = snap.plans_computed;
            prev_execs = snap.executions_run;
            prev_removed = snap.segments_removed;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        plans in 0u64..10000,
        execs in 0u64..10000,
        evaluated in 0u64..50000,
        targets in 0u64..10000,
        planned in 0u64..100000,
        removed in 0u64..100000,
        errors in 0u64..5000,
    ) {
        let snap = ScrollbackEvictionTelemetrySnapshot {
            plans_computed: plans,
            executions_run: execs,
            panes_evaluated: evaluated,
            targets_generated: targets,
            segments_planned: planned,
            segments_removed: removed,
            execution_errors: errors,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: ScrollbackEvictionTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }

    #[test]
    fn segments_removed_bounded_by_planned(
        pane_segments in prop::collection::vec(100usize..1000, 1..10),
    ) {
        let panes: Vec<(u64, usize)> = pane_segments
            .iter()
            .enumerate()
            .map(|(i, &segs)| (i as u64, segs))
            .collect();
        let store = MockStore::new(panes);
        let evictor = ScrollbackEvictor::new(
            EvictionConfig::default(),
            store,
            AllDormantTierSource,
        );

        let _ = evictor.evict(MemoryPressureTier::Green);
        let snap = evictor.telemetry().snapshot();

        prop_assert!(
            snap.segments_removed <= snap.segments_planned,
            "segments_removed ({}) should be <= segments_planned ({})",
            snap.segments_removed, snap.segments_planned
        );
    }

    #[test]
    fn pressure_affects_targets(
        pane_count in 1usize..10,
    ) {
        // Under higher pressure, more panes should need eviction
        let panes: Vec<(u64, usize)> = (0..pane_count as u64)
            .map(|id| (id, 300)) // 300 segments each
            .collect();

        // Green: dormant_max = 100, all need eviction
        let store_green = MockStore::new(panes.clone());
        let evictor_green = ScrollbackEvictor::new(
            EvictionConfig::default(),
            store_green,
            CyclingTierSource,
        );
        let plan_green = evictor_green.plan(MemoryPressureTier::Green).unwrap();

        // Red: even Active panes get trimmed (max = min(10000/4, 200) = 200)
        let store_red = MockStore::new(panes);
        let evictor_red = ScrollbackEvictor::new(
            EvictionConfig::default(),
            store_red,
            CyclingTierSource,
        );
        let plan_red = evictor_red.plan(MemoryPressureTier::Red).unwrap();

        prop_assert!(
            plan_red.total_segments_to_remove >= plan_green.total_segments_to_remove,
            "Red pressure ({}) should remove >= Green pressure ({})",
            plan_red.total_segments_to_remove, plan_green.total_segments_to_remove
        );
    }
}
