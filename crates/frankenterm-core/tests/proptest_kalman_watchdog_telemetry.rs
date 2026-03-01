//! Property-based tests for adaptive watchdog telemetry counters (ft-3kxe.19).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. observations / observations_matched track observe() calls
//! 3. health_checks tracks check_health() calls
//! 4. classifications tracks classify_component() calls
//! 5. resets tracks reset() calls
//! 6. status_* counters track classification outcomes
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::kalman_watchdog::{
    AdaptiveWatchdog, AdaptiveWatchdogConfig, AdaptiveWatchdogTelemetrySnapshot,
};
use frankenterm_core::watchdog::Component;

// =============================================================================
// Helpers
// =============================================================================

fn make_watchdog() -> AdaptiveWatchdog {
    AdaptiveWatchdog::new(AdaptiveWatchdogConfig::default())
}

fn _make_fast_warmup_watchdog() -> AdaptiveWatchdog {
    AdaptiveWatchdog::new(AdaptiveWatchdogConfig {
        min_observations: 2,
        ..AdaptiveWatchdogConfig::default()
    })
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let wd = make_watchdog();
    let snap = wd.telemetry().snapshot();

    assert_eq!(snap.observations, 0);
    assert_eq!(snap.observations_matched, 0);
    assert_eq!(snap.health_checks, 0);
    assert_eq!(snap.classifications, 0);
    assert_eq!(snap.resets, 0);
    assert_eq!(snap.status_healthy, 0);
    assert_eq!(snap.status_degraded, 0);
    assert_eq!(snap.status_critical, 0);
    assert_eq!(snap.status_hung, 0);
}

#[test]
fn observe_increments_counters() {
    let mut wd = make_watchdog();

    wd.observe(Component::Capture, 1000);
    wd.observe(Component::Capture, 2000);
    wd.observe(Component::Discovery, 1500);

    let snap = wd.telemetry().snapshot();
    assert_eq!(snap.observations, 3);
    assert_eq!(snap.observations_matched, 3);
}

#[test]
fn observe_unmatched_component() {
    let wd_config = AdaptiveWatchdogConfig::default();
    let mut wd = AdaptiveWatchdog::with_fallbacks(wd_config, &[(Component::Capture, 5000)]);

    wd.observe(Component::Capture, 1000); // matched
    wd.observe(Component::Discovery, 2000); // NOT matched (not in fallbacks)

    let snap = wd.telemetry().snapshot();
    assert_eq!(snap.observations, 2);
    assert_eq!(snap.observations_matched, 1);
}

#[test]
fn check_health_increments_counter() {
    let mut wd = make_watchdog();

    let _ = wd.check_health(1000);
    let _ = wd.check_health(2000);
    let _ = wd.check_health(3000);

    let snap = wd.telemetry().snapshot();
    assert_eq!(snap.health_checks, 3);
}

#[test]
fn check_health_counts_statuses() {
    let mut wd = make_watchdog();

    // All components are in warmup with no heartbeats → Healthy by default
    let _ = wd.check_health(1000);

    let snap = wd.telemetry().snapshot();
    // 4 components (Discovery, Capture, Persistence, Maintenance) all Healthy
    assert_eq!(snap.status_healthy, 4);
    assert_eq!(snap.status_degraded, 0);
}

#[test]
fn classify_component_increments() {
    let mut wd = make_watchdog();
    wd.observe(Component::Capture, 1000);

    let _ = wd.classify_component(Component::Capture, 2000);
    let _ = wd.classify_component(Component::Capture, 3000);

    let snap = wd.telemetry().snapshot();
    assert_eq!(snap.classifications, 2);
}

#[test]
fn classify_nonexistent_still_counts() {
    let mut wd = make_watchdog();

    // Discovery is registered by default, but let's try a custom setup
    let wd_config = AdaptiveWatchdogConfig::default();
    let mut wd2 = AdaptiveWatchdog::with_fallbacks(wd_config, &[(Component::Capture, 5000)]);

    let result = wd2.classify_component(Component::Discovery, 1000);
    assert!(result.is_none());

    let snap = wd2.telemetry().snapshot();
    assert_eq!(snap.classifications, 1);
    // No status counted since result was None
    assert_eq!(snap.status_healthy, 0);

    // Now classify an existing component
    let _ = wd.classify_component(Component::Capture, 1000);
    let snap = wd.telemetry().snapshot();
    assert_eq!(snap.classifications, 1);
    assert_eq!(snap.status_healthy, 1);
}

#[test]
fn reset_increments() {
    let mut wd = make_watchdog();

    wd.reset();
    wd.reset();

    let snap = wd.telemetry().snapshot();
    assert_eq!(snap.resets, 2);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = AdaptiveWatchdogTelemetrySnapshot {
        observations: 100,
        observations_matched: 90,
        health_checks: 50,
        classifications: 30,
        resets: 5,
        status_healthy: 150,
        status_degraded: 20,
        status_critical: 8,
        status_hung: 2,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: AdaptiveWatchdogTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn mixed_operations() {
    let mut wd = make_watchdog();

    // Observe some heartbeats
    wd.observe(Component::Capture, 1000);
    wd.observe(Component::Capture, 2000);
    wd.observe(Component::Discovery, 1500);

    // Health check
    let _ = wd.check_health(3000);

    // Single classification
    let _ = wd.classify_component(Component::Capture, 3500);

    // Reset
    wd.reset();

    let snap = wd.telemetry().snapshot();
    assert_eq!(snap.observations, 3);
    assert_eq!(snap.observations_matched, 3);
    assert_eq!(snap.health_checks, 1);
    assert_eq!(snap.classifications, 1);
    assert_eq!(snap.resets, 1);
    // 4 from check_health + 1 from classify = 5 status counts total
    assert!(
        snap.status_healthy + snap.status_degraded + snap.status_critical + snap.status_hung == 5
    );
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn observations_equals_call_count(
        count in 1usize..50,
    ) {
        let mut wd = make_watchdog();
        for i in 0..count {
            wd.observe(Component::Capture, i as u64 * 1000);
        }
        let snap = wd.telemetry().snapshot();
        prop_assert_eq!(snap.observations, count as u64);
        prop_assert_eq!(snap.observations_matched, count as u64);
    }

    #[test]
    fn health_checks_equals_call_count(
        count in 1usize..20,
    ) {
        let mut wd = make_watchdog();
        for i in 0..count {
            let _ = wd.check_health(i as u64 * 1000);
        }
        let snap = wd.telemetry().snapshot();
        prop_assert_eq!(snap.health_checks, count as u64);
    }

    #[test]
    fn status_counts_equal_classified_components(
        health_check_count in 1usize..10,
    ) {
        let mut wd = make_watchdog();
        for i in 0..health_check_count {
            let _ = wd.check_health(i as u64 * 1000);
        }
        let snap = wd.telemetry().snapshot();
        let total_statuses = snap.status_healthy + snap.status_degraded
            + snap.status_critical + snap.status_hung;
        // Each check_health classifies all 4 components
        prop_assert_eq!(total_statuses, (health_check_count * 4) as u64,
            "total statuses ({}) != health_checks * 4 ({})",
            total_statuses, health_check_count * 4);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..4, 1..30),
    ) {
        let mut wd = make_watchdog();
        let mut prev = wd.telemetry().snapshot();

        for (i, op) in ops.iter().enumerate() {
            match op {
                0 => { wd.observe(Component::Capture, i as u64 * 1000); }
                1 => { let _ = wd.check_health(i as u64 * 1000); }
                2 => { let _ = wd.classify_component(Component::Capture, i as u64 * 1000); }
                3 => { wd.reset(); }
                _ => unreachable!(),
            }

            let snap = wd.telemetry().snapshot();
            prop_assert!(snap.observations >= prev.observations,
                "observations decreased: {} -> {}",
                prev.observations, snap.observations);
            prop_assert!(snap.health_checks >= prev.health_checks,
                "health_checks decreased: {} -> {}",
                prev.health_checks, snap.health_checks);
            prop_assert!(snap.classifications >= prev.classifications,
                "classifications decreased: {} -> {}",
                prev.classifications, snap.classifications);
            prop_assert!(snap.resets >= prev.resets,
                "resets decreased: {} -> {}",
                prev.resets, snap.resets);
            prop_assert!(snap.status_healthy >= prev.status_healthy,
                "status_healthy decreased: {} -> {}",
                prev.status_healthy, snap.status_healthy);
            prop_assert!(snap.status_degraded >= prev.status_degraded,
                "status_degraded decreased: {} -> {}",
                prev.status_degraded, snap.status_degraded);

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        obs in 0u64..50000,
        matched in 0u64..50000,
        hc in 0u64..10000,
        cls in 0u64..10000,
        resets in 0u64..5000,
        healthy in 0u64..50000,
        degraded in 0u64..10000,
        critical in 0u64..5000,
    ) {
        let snap = AdaptiveWatchdogTelemetrySnapshot {
            observations: obs,
            observations_matched: matched,
            health_checks: hc,
            classifications: cls,
            resets,
            status_healthy: healthy,
            status_degraded: degraded,
            status_critical: critical,
            status_hung: 0,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: AdaptiveWatchdogTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
