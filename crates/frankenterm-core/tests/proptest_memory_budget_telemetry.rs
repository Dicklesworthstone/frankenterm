//! Property-based tests for memory budget telemetry counters (ft-3kxe.24).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. panes_registered tracks register_pane() calls
//! 3. panes_unregistered tracks unregister_pane() calls
//! 4. samples tracks sample_all() calls
//! 5. samples_with_pressure tracks samples detecting budget pressure
//! 6. Serde roundtrip for snapshot
//! 7. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::memory_budget::{
    MemoryBudgetConfig, MemoryBudgetManager, MemoryBudgetTelemetrySnapshot,
};

// =============================================================================
// Helpers
// =============================================================================

fn test_config() -> MemoryBudgetConfig {
    MemoryBudgetConfig {
        enabled: true,
        default_budget_bytes: 1_000_000,
        high_ratio: 0.8,
        sample_interval_ms: 1000,
        cgroup_base_path: "/nonexistent".to_string(),
        use_cgroups: false,
        oom_score_adj: 0,
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let mgr = MemoryBudgetManager::new(test_config());
    let snap = mgr.telemetry().snapshot();

    assert_eq!(snap.panes_registered, 0);
    assert_eq!(snap.panes_unregistered, 0);
    assert_eq!(snap.samples, 0);
    assert_eq!(snap.samples_with_pressure, 0);
}

#[test]
fn panes_registered_tracked() {
    let mgr = MemoryBudgetManager::new(test_config());
    mgr.register_pane(1, None);
    mgr.register_pane(2, None);
    mgr.register_pane(3, Some(999));

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 3);
}

#[test]
fn register_with_custom_budget_tracked() {
    let mgr = MemoryBudgetManager::new(test_config());
    mgr.register_pane_with_budget(1, None, 512_000);
    mgr.register_pane_with_budget(2, None, 1_024_000);

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 2);
}

#[test]
fn panes_unregistered_tracked() {
    let mgr = MemoryBudgetManager::new(test_config());
    mgr.register_pane(1, None);
    mgr.register_pane(2, None);
    mgr.unregister_pane(1);
    mgr.unregister_pane(2);

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_unregistered, 2);
}

#[test]
fn unregister_missing_pane_still_counts() {
    let mgr = MemoryBudgetManager::new(test_config());
    mgr.unregister_pane(999);

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_unregistered, 1);
}

#[test]
fn samples_tracked() {
    let mgr = MemoryBudgetManager::new(test_config());
    mgr.register_pane(1, None);
    mgr.sample_all();
    mgr.sample_all();
    mgr.sample_all();

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.samples, 3);
}

#[test]
fn samples_empty_manager_no_pressure() {
    let mgr = MemoryBudgetManager::new(test_config());
    mgr.sample_all();

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.samples, 1);
    assert_eq!(snap.samples_with_pressure, 0);
}

#[test]
fn samples_no_pressure_when_normal() {
    let mgr = MemoryBudgetManager::new(test_config());
    // Register pane with no PID — usage will be 0 (Normal)
    mgr.register_pane(1, None);
    mgr.sample_all();

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.samples, 1);
    assert_eq!(snap.samples_with_pressure, 0);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = MemoryBudgetTelemetrySnapshot {
        panes_registered: 100,
        panes_unregistered: 50,
        samples: 1000,
        samples_with_pressure: 25,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: MemoryBudgetTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn mixed_operations() {
    let mgr = MemoryBudgetManager::new(test_config());

    mgr.register_pane(1, None);
    mgr.register_pane(2, None);
    mgr.sample_all();
    mgr.unregister_pane(1);
    mgr.sample_all();
    mgr.register_pane(3, None);
    mgr.sample_all();

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 3);
    assert_eq!(snap.panes_unregistered, 1);
    assert_eq!(snap.samples, 3);
}

#[test]
fn register_same_pane_id_twice_counts_both() {
    let mgr = MemoryBudgetManager::new(test_config());
    mgr.register_pane(1, None);
    mgr.register_pane(1, None); // re-register same pane

    let snap = mgr.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 2);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn panes_registered_equals_call_count(
        count in 1usize..30,
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        for i in 0..count {
            mgr.register_pane(i as u64, None);
        }
        let snap = mgr.telemetry().snapshot();
        prop_assert_eq!(snap.panes_registered, count as u64);
    }

    #[test]
    fn samples_equals_call_count(
        count in 1usize..30,
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        mgr.register_pane(1, None);
        for _ in 0..count {
            mgr.sample_all();
        }
        let snap = mgr.telemetry().snapshot();
        prop_assert_eq!(snap.samples, count as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..4, 1..30),
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        let mut prev = mgr.telemetry().snapshot();
        let mut next_id = 1u64;

        for op in &ops {
            match op {
                0 => { mgr.register_pane(next_id, None); next_id += 1; }
                1 => { mgr.unregister_pane(next_id.saturating_sub(1)); }
                2 => { mgr.sample_all(); }
                3 => { mgr.register_pane_with_budget(next_id, None, 500_000); next_id += 1; }
                _ => unreachable!(),
            }

            let snap = mgr.telemetry().snapshot();
            prop_assert!(snap.panes_registered >= prev.panes_registered,
                "panes_registered decreased: {} -> {}",
                prev.panes_registered, snap.panes_registered);
            prop_assert!(snap.panes_unregistered >= prev.panes_unregistered,
                "panes_unregistered decreased: {} -> {}",
                prev.panes_unregistered, snap.panes_unregistered);
            prop_assert!(snap.samples >= prev.samples,
                "samples decreased: {} -> {}",
                prev.samples, snap.samples);
            prop_assert!(snap.samples_with_pressure >= prev.samples_with_pressure,
                "samples_with_pressure decreased: {} -> {}",
                prev.samples_with_pressure, snap.samples_with_pressure);

            prev = snap;
        }
    }

    #[test]
    fn samples_with_pressure_bounded_by_samples(
        count in 1usize..30,
    ) {
        let mgr = MemoryBudgetManager::new(test_config());
        mgr.register_pane(1, None);
        for _ in 0..count {
            mgr.sample_all();
        }
        let snap = mgr.telemetry().snapshot();
        prop_assert!(
            snap.samples_with_pressure <= snap.samples,
            "samples_with_pressure ({}) > samples ({})",
            snap.samples_with_pressure, snap.samples
        );
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        registered in 0u64..100000,
        unregistered in 0u64..50000,
        samples in 0u64..100000,
        pressure in 0u64..50000,
    ) {
        let snap = MemoryBudgetTelemetrySnapshot {
            panes_registered: registered,
            panes_unregistered: unregistered,
            samples,
            samples_with_pressure: pressure,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: MemoryBudgetTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
