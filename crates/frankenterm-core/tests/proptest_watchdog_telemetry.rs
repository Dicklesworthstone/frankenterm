//! Property-based tests for watchdog telemetry counters (ft-3kxe.38).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. discovery_heartbeats tracks record_discovery() calls
//! 3. capture_heartbeats tracks record_capture() calls
//! 4. persistence_heartbeats tracks record_persistence() calls
//! 5. maintenance_heartbeats tracks record_maintenance() calls
//! 6. health_checks tracks check_health() calls
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::watchdog::{HeartbeatRegistry, WatchdogConfig, WatchdogTelemetrySnapshot};

// =============================================================================
// Helpers
// =============================================================================

fn test_registry() -> HeartbeatRegistry {
    HeartbeatRegistry::new()
}

fn test_config() -> WatchdogConfig {
    WatchdogConfig::default()
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let reg = test_registry();
    let snap = reg.telemetry().snapshot();

    assert_eq!(snap.discovery_heartbeats, 0);
    assert_eq!(snap.capture_heartbeats, 0);
    assert_eq!(snap.persistence_heartbeats, 0);
    assert_eq!(snap.maintenance_heartbeats, 0);
    assert_eq!(snap.health_checks, 0);
}

#[test]
fn discovery_heartbeats_tracked() {
    let reg = test_registry();
    reg.record_discovery();
    reg.record_discovery();
    reg.record_discovery();

    let snap = reg.telemetry().snapshot();
    assert_eq!(snap.discovery_heartbeats, 3);
    assert_eq!(snap.capture_heartbeats, 0);
}

#[test]
fn capture_heartbeats_tracked() {
    let reg = test_registry();
    reg.record_capture();
    reg.record_capture();

    let snap = reg.telemetry().snapshot();
    assert_eq!(snap.capture_heartbeats, 2);
    assert_eq!(snap.discovery_heartbeats, 0);
}

#[test]
fn persistence_heartbeats_tracked() {
    let reg = test_registry();
    reg.record_persistence();

    let snap = reg.telemetry().snapshot();
    assert_eq!(snap.persistence_heartbeats, 1);
}

#[test]
fn maintenance_heartbeats_tracked() {
    let reg = test_registry();
    reg.record_maintenance();
    reg.record_maintenance();

    let snap = reg.telemetry().snapshot();
    assert_eq!(snap.maintenance_heartbeats, 2);
}

#[test]
fn health_checks_tracked() {
    let reg = test_registry();
    let config = test_config();
    reg.check_health(&config);
    reg.check_health(&config);

    let snap = reg.telemetry().snapshot();
    assert_eq!(snap.health_checks, 2);
}

#[test]
fn mixed_operations_tracked() {
    let reg = test_registry();
    let config = test_config();

    reg.record_discovery();
    reg.record_capture();
    reg.record_persistence();
    reg.record_maintenance();
    reg.check_health(&config);

    let snap = reg.telemetry().snapshot();
    assert_eq!(snap.discovery_heartbeats, 1);
    assert_eq!(snap.capture_heartbeats, 1);
    assert_eq!(snap.persistence_heartbeats, 1);
    assert_eq!(snap.maintenance_heartbeats, 1);
    assert_eq!(snap.health_checks, 1);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = WatchdogTelemetrySnapshot {
        discovery_heartbeats: 1000,
        capture_heartbeats: 2000,
        persistence_heartbeats: 500,
        maintenance_heartbeats: 300,
        health_checks: 100,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: WatchdogTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn counters_independent() {
    let reg = test_registry();
    for _ in 0..10 {
        reg.record_discovery();
    }
    for _ in 0..5 {
        reg.record_capture();
    }

    let snap = reg.telemetry().snapshot();
    assert_eq!(snap.discovery_heartbeats, 10);
    assert_eq!(snap.capture_heartbeats, 5);
    assert_eq!(snap.persistence_heartbeats, 0);
    assert_eq!(snap.maintenance_heartbeats, 0);
    assert_eq!(snap.health_checks, 0);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn heartbeats_equal_call_count(
        discovery in 0usize..20,
        capture in 0usize..20,
        persistence in 0usize..20,
        maintenance in 0usize..20,
    ) {
        let reg = test_registry();
        for _ in 0..discovery {
            reg.record_discovery();
        }
        for _ in 0..capture {
            reg.record_capture();
        }
        for _ in 0..persistence {
            reg.record_persistence();
        }
        for _ in 0..maintenance {
            reg.record_maintenance();
        }

        let snap = reg.telemetry().snapshot();
        prop_assert_eq!(snap.discovery_heartbeats, discovery as u64);
        prop_assert_eq!(snap.capture_heartbeats, capture as u64);
        prop_assert_eq!(snap.persistence_heartbeats, persistence as u64);
        prop_assert_eq!(snap.maintenance_heartbeats, maintenance as u64);
    }

    #[test]
    fn health_checks_equal_call_count(
        checks in 0usize..20,
    ) {
        let reg = test_registry();
        let config = test_config();
        for _ in 0..checks {
            reg.check_health(&config);
        }

        let snap = reg.telemetry().snapshot();
        prop_assert_eq!(snap.health_checks, checks as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..5, 1..30),
    ) {
        let reg = test_registry();
        let config = test_config();
        let mut prev = reg.telemetry().snapshot();

        for op in &ops {
            match op {
                0 => reg.record_discovery(),
                1 => reg.record_capture(),
                2 => reg.record_persistence(),
                3 => reg.record_maintenance(),
                4 => { reg.check_health(&config); }
                _ => unreachable!(),
            }

            let snap = reg.telemetry().snapshot();
            prop_assert!(snap.discovery_heartbeats >= prev.discovery_heartbeats,
                "discovery_heartbeats decreased");
            prop_assert!(snap.capture_heartbeats >= prev.capture_heartbeats,
                "capture_heartbeats decreased");
            prop_assert!(snap.persistence_heartbeats >= prev.persistence_heartbeats,
                "persistence_heartbeats decreased");
            prop_assert!(snap.maintenance_heartbeats >= prev.maintenance_heartbeats,
                "maintenance_heartbeats decreased");
            prop_assert!(snap.health_checks >= prev.health_checks,
                "health_checks decreased");

            prev = snap;
        }
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        discovery_heartbeats in 0u64..100000,
        capture_heartbeats in 0u64..100000,
        persistence_heartbeats in 0u64..100000,
        maintenance_heartbeats in 0u64..100000,
        health_checks in 0u64..10000,
    ) {
        let snap = WatchdogTelemetrySnapshot {
            discovery_heartbeats,
            capture_heartbeats,
            persistence_heartbeats,
            maintenance_heartbeats,
            health_checks,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: WatchdogTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
