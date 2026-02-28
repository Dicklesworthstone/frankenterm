//! Property-based tests for pane lifecycle engine telemetry counters (ft-3kxe.17).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. health_checks counts all health_check() calls
//! 3. panes_registered counts new pane IDs
//! 4. panes_removed counts remove_pane() calls
//! 5. Action counters track recommended actions
//! 6. Serde roundtrip for snapshot
//! 7. Counter monotonicity across operations

use proptest::prelude::*;
use std::time::Duration;

use frankenterm_core::pane_lifecycle::{
    LifecycleConfig, PaneLifecycleEngine, PaneLifecycleTelemetrySnapshot,
};

// =============================================================================
// Helpers
// =============================================================================

fn make_engine() -> PaneLifecycleEngine {
    PaneLifecycleEngine::with_defaults()
}

/// Young healthy pane parameters: <4h, >10% CPU → Active → action None.
fn young_active_params() -> (Duration, f64) {
    (Duration::from_secs(3600), 15.0) // 1h, 15% CPU
}

/// Mid-age low-CPU pane: 4-16h, <2% CPU → PossiblyStuck → Warn/Review.
fn midage_stuck_params() -> (Duration, f64) {
    (Duration::from_secs(8 * 3600), 1.0) // 8h, 1% CPU
}

/// Old pane: 16-24h → LikelyStuck → GracefulKill.
fn old_stuck_params() -> (Duration, f64) {
    (Duration::from_secs(20 * 3600), 0.5) // 20h, 0.5% CPU
}

/// Abandoned pane: >24h → Abandoned → ForceKill.
fn abandoned_params() -> (Duration, f64) {
    (Duration::from_secs(30 * 3600), 0.1) // 30h, 0.1% CPU
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let engine = make_engine();
    let snap = engine.telemetry().snapshot();

    assert_eq!(snap.health_checks, 0);
    assert_eq!(snap.panes_registered, 0);
    assert_eq!(snap.panes_removed, 0);
    assert_eq!(snap.actions_none, 0);
    assert_eq!(snap.actions_warn, 0);
    assert_eq!(snap.actions_review, 0);
    assert_eq!(snap.actions_graceful_kill, 0);
    assert_eq!(snap.actions_force_kill, 0);
}

#[test]
fn health_check_increments_counter() {
    let mut engine = make_engine();
    let (age, cpu) = young_active_params();

    engine.health_check(1, 100, age, cpu, None);
    engine.health_check(1, 100, age, cpu, None);
    engine.health_check(2, 200, age, cpu, None);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.health_checks, 3);
}

#[test]
fn new_panes_tracked() {
    let mut engine = make_engine();
    let (age, cpu) = young_active_params();

    engine.health_check(1, 100, age, cpu, None);
    engine.health_check(2, 200, age, cpu, None);
    engine.health_check(1, 100, age, cpu, None); // repeat pane 1

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.panes_registered, 2); // only 2 unique panes
}

#[test]
fn remove_pane_tracked() {
    let mut engine = make_engine();
    let (age, cpu) = young_active_params();

    engine.health_check(1, 100, age, cpu, None);
    engine.health_check(2, 200, age, cpu, None);
    engine.remove_pane(1);
    engine.remove_pane(99); // nonexistent — should not count

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.panes_removed, 1);
}

#[test]
fn active_pane_gets_none_action() {
    let mut engine = make_engine();
    let (age, cpu) = young_active_params();

    engine.health_check(1, 100, age, cpu, None);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.actions_none, 1);
}

#[test]
fn stuck_pane_gets_warn_then_review() {
    let mut engine = make_engine();
    let (age, cpu) = midage_stuck_params();

    // First check → Warn (possibly stuck, first time)
    engine.health_check(1, 100, age, cpu, None);
    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.actions_warn, 1);

    // Second check → Review (warned before)
    engine.health_check(1, 100, age, cpu, None);
    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.actions_review, 1);
}

#[test]
fn old_pane_gets_graceful_kill() {
    let mut engine = make_engine();
    let (age, cpu) = old_stuck_params();

    engine.health_check(1, 100, age, cpu, None);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.actions_graceful_kill, 1);
}

#[test]
fn abandoned_pane_gets_force_kill() {
    let mut engine = make_engine();
    let (age, cpu) = abandoned_params();

    engine.health_check(1, 100, age, cpu, None);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.actions_force_kill, 1);
}

#[test]
fn protected_pane_gets_none_action() {
    let mut engine = PaneLifecycleEngine::new(LifecycleConfig {
        protected_panes: vec![1],
        ..LifecycleConfig::default()
    });
    let (age, cpu) = abandoned_params(); // would be ForceKill if not protected

    engine.health_check(1, 100, age, cpu, None);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.actions_none, 1);
    assert_eq!(snap.actions_force_kill, 0);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = PaneLifecycleTelemetrySnapshot {
        health_checks: 50,
        panes_registered: 10,
        panes_removed: 3,
        actions_none: 30,
        actions_warn: 5,
        actions_review: 7,
        actions_graceful_kill: 3,
        actions_force_kill: 2,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: PaneLifecycleTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn health_checks_equals_call_count(
        pane_ids in prop::collection::vec(1u64..10, 1..30),
    ) {
        let mut engine = make_engine();
        let (age, cpu) = young_active_params();

        for &pid in &pane_ids {
            engine.health_check(pid, 100, age, cpu, None);
        }

        let snap = engine.telemetry().snapshot();
        prop_assert_eq!(snap.health_checks, pane_ids.len() as u64);
    }

    #[test]
    fn panes_registered_counts_unique_ids(
        pane_ids in prop::collection::vec(1u64..5, 1..20),
    ) {
        let mut engine = make_engine();
        let (age, cpu) = young_active_params();

        for &pid in &pane_ids {
            engine.health_check(pid, 100, age, cpu, None);
        }

        let snap = engine.telemetry().snapshot();
        let unique_count = pane_ids.iter().collect::<std::collections::HashSet<_>>().len();
        prop_assert_eq!(snap.panes_registered, unique_count as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        pane_ids in prop::collection::vec(1u64..10, 1..20),
    ) {
        let mut engine = make_engine();
        let (age, cpu) = young_active_params();
        let mut prev = engine.telemetry().snapshot();

        for &pid in &pane_ids {
            engine.health_check(pid, 100, age, cpu, None);

            let snap = engine.telemetry().snapshot();
            prop_assert!(snap.health_checks >= prev.health_checks,
                "health_checks decreased: {} -> {}", prev.health_checks, snap.health_checks);
            prop_assert!(snap.panes_registered >= prev.panes_registered,
                "panes_registered decreased: {} -> {}", prev.panes_registered, snap.panes_registered);
            prop_assert!(snap.actions_none >= prev.actions_none,
                "actions_none decreased: {} -> {}", prev.actions_none, snap.actions_none);

            prev = snap;
        }
    }

    #[test]
    fn action_counts_sum_to_health_checks(
        pane_ids in prop::collection::vec(1u64..10, 1..20),
    ) {
        let mut engine = make_engine();
        let (age, cpu) = young_active_params();

        for &pid in &pane_ids {
            engine.health_check(pid, 100, age, cpu, None);
        }

        let snap = engine.telemetry().snapshot();
        let total_actions = snap.actions_none + snap.actions_warn + snap.actions_review
            + snap.actions_graceful_kill + snap.actions_force_kill;
        prop_assert_eq!(
            total_actions, snap.health_checks,
            "action counts ({}) should equal health_checks ({})",
            total_actions, snap.health_checks
        );
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        hc in 0u64..50000,
        reg in 0u64..10000,
        rem in 0u64..10000,
        none in 0u64..20000,
        warn in 0u64..5000,
        review in 0u64..5000,
        gk in 0u64..5000,
        fk in 0u64..5000,
    ) {
        let snap = PaneLifecycleTelemetrySnapshot {
            health_checks: hc,
            panes_registered: reg,
            panes_removed: rem,
            actions_none: none,
            actions_warn: warn,
            actions_review: review,
            actions_graceful_kill: gk,
            actions_force_kill: fk,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: PaneLifecycleTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
