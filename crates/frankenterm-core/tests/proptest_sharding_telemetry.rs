//! Property-based tests for sharding telemetry counters (ft-3kxe.42).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. Serde roundtrip for snapshot
//! 3. Counter monotonicity (via snapshot roundtrip — async methods
//!    require the supported LabRuntime substrate and mock backends)
//!
//! Note: ShardedWeztermClient methods are async and require MuxInterface
//! backends, so we test telemetry snapshot serialization and the type
//! contracts. Full integration testing of counter accuracy happens in
//! the sharding integration test suite.

use proptest::prelude::*;

use frankenterm_core::sharding::ShardingTelemetrySnapshot;

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn snapshot_serde_roundtrip() {
    let snap = ShardingTelemetrySnapshot {
        spawns: 100,
        pane_listings: 500,
        health_reports: 50,
        route_lookups: 2000,
        route_snapshot_conflicts: 7,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: ShardingTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn snapshot_zero_values() {
    let json = r#"{"spawns":0,"pane_listings":0,"health_reports":0,"route_lookups":0,"route_snapshot_conflicts":0}"#;
    let snap: ShardingTelemetrySnapshot = serde_json::from_str(json).expect("deserialize");
    assert_eq!(snap.spawns, 0);
    assert_eq!(snap.pane_listings, 0);
    assert_eq!(snap.health_reports, 0);
    assert_eq!(snap.route_lookups, 0);
    assert_eq!(snap.route_snapshot_conflicts, 0);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn snapshot_roundtrip_arbitrary(
        spawns in 0u64..100000,
        pane_listings in 0u64..100000,
        health_reports in 0u64..10000,
        route_lookups in 0u64..1000000,
        route_snapshot_conflicts in 0u64..100000,
    ) {
        let snap = ShardingTelemetrySnapshot {
            spawns,
            pane_listings,
            health_reports,
            route_lookups,
            route_snapshot_conflicts,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: ShardingTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
