//! Property-based tests for correlation engine telemetry counters (ft-3kxe.26).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. events_ingested tracks ingest()/ingest_batch() calls
//! 3. scans tracks scan() calls
//! 4. correlations_found tracks total correlations across scans
//! 5. prunes tracks prune()/scan() calls
//! 6. events_pruned tracks events removed
//! 7. Serde roundtrip for snapshot
//! 8. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::cross_pane_correlation::{
    CorrelationConfig, CorrelationEngine, CorrelationEngineTelemetrySnapshot, EventRecord,
};

// =============================================================================
// Helpers
// =============================================================================

fn test_config() -> CorrelationConfig {
    CorrelationConfig {
        window_ms: 1000,
        min_observations: 2,
        p_value_threshold: 0.05,
        max_event_types: 50,
        retention_ms: 10_000,
        max_panes: 100,
    }
}

fn make_event(pane_id: u64, event_type: &str, timestamp_ms: u64) -> EventRecord {
    EventRecord {
        pane_id,
        event_type: event_type.to_string(),
        timestamp_ms,
    }
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let engine = CorrelationEngine::new(test_config());
    let snap = engine.telemetry().snapshot();

    assert_eq!(snap.events_ingested, 0);
    assert_eq!(snap.scans, 0);
    assert_eq!(snap.correlations_found, 0);
    assert_eq!(snap.prunes, 0);
    assert_eq!(snap.events_pruned, 0);
}

#[test]
fn ingest_tracked() {
    let mut engine = CorrelationEngine::new(test_config());
    engine.ingest(make_event(1, "error", 1000));
    engine.ingest(make_event(2, "error", 1001));
    engine.ingest(make_event(1, "rate_limit", 1002));

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.events_ingested, 3);
}

#[test]
fn ingest_batch_tracked() {
    let mut engine = CorrelationEngine::new(test_config());
    let events = vec![
        make_event(1, "error", 1000),
        make_event(2, "error", 1001),
        make_event(3, "rate_limit", 1002),
    ];
    engine.ingest_batch(events);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.events_ingested, 3);
}

#[test]
fn mixed_ingest_tracked() {
    let mut engine = CorrelationEngine::new(test_config());
    engine.ingest(make_event(1, "error", 1000));
    engine.ingest_batch(vec![
        make_event(2, "error", 1001),
        make_event(3, "rate_limit", 1002),
    ]);
    engine.ingest(make_event(4, "timeout", 1003));

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.events_ingested, 4);
}

#[test]
fn scan_tracked() {
    let mut engine = CorrelationEngine::new(test_config());
    engine.ingest(make_event(1, "error", 1000));
    engine.scan(2000);
    engine.scan(3000);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.scans, 2);
}

#[test]
fn prune_tracked() {
    let mut engine = CorrelationEngine::new(test_config());
    engine.ingest(make_event(1, "error", 1000));
    engine.ingest(make_event(2, "error", 2000));

    // Prune at time that makes first event expire (retention=10_000)
    engine.prune(12_000);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.prunes, 1);
    assert_eq!(snap.events_pruned, 1);
}

#[test]
fn prune_nothing_still_counts() {
    let mut engine = CorrelationEngine::new(test_config());
    engine.ingest(make_event(1, "error", 5000));

    // All events within retention window
    engine.prune(6000);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.prunes, 1);
    assert_eq!(snap.events_pruned, 0);
}

#[test]
fn scan_triggers_prune() {
    let mut engine = CorrelationEngine::new(test_config());
    engine.ingest(make_event(1, "error", 1000));

    // Scan at time that makes event expire
    engine.scan(12_000);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.scans, 1);
    // scan calls prune internally
    assert_eq!(snap.prunes, 1);
    assert_eq!(snap.events_pruned, 1);
}

#[test]
fn correlations_found_tracked() {
    let mut engine = CorrelationEngine::new(test_config());

    // Create a pattern of co-occurring events across multiple windows
    for i in 0..20 {
        let ts = 1000 + i * 500;
        engine.ingest(make_event(1, "error", ts));
        engine.ingest(make_event(2, "rate_limit", ts + 10));
    }

    let results = engine.scan(15_000);
    let snap = engine.telemetry().snapshot();

    assert_eq!(snap.scans, 1);
    assert_eq!(snap.correlations_found, results.len() as u64);
}

#[test]
fn empty_ingest_batch_no_increment() {
    let mut engine = CorrelationEngine::new(test_config());
    engine.ingest_batch(Vec::<EventRecord>::new());

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.events_ingested, 0);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = CorrelationEngineTelemetrySnapshot {
        events_ingested: 1000,
        scans: 50,
        correlations_found: 25,
        prunes: 30,
        events_pruned: 500,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: CorrelationEngineTelemetrySnapshot =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn events_ingested_equals_call_count(
        count in 1usize..50,
    ) {
        let mut engine = CorrelationEngine::new(test_config());
        for i in 0..count {
            engine.ingest(make_event(i as u64, "event", 1000 + i as u64));
        }
        let snap = engine.telemetry().snapshot();
        prop_assert_eq!(snap.events_ingested, count as u64);
    }

    #[test]
    fn scans_equals_call_count(
        count in 1usize..20,
    ) {
        let mut engine = CorrelationEngine::new(test_config());
        engine.ingest(make_event(1, "event", 1000));
        for i in 0..count {
            engine.scan(2000 + i as u64 * 1000);
        }
        let snap = engine.telemetry().snapshot();
        prop_assert_eq!(snap.scans, count as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(0u8..4, 1..30),
    ) {
        let mut engine = CorrelationEngine::new(test_config());
        let mut prev = engine.telemetry().snapshot();
        let mut time = 1000u64;

        for op in &ops {
            match op {
                0 => {
                    engine.ingest(make_event(1, "event_a", time));
                    time += 100;
                }
                1 => {
                    engine.ingest_batch(vec![
                        make_event(1, "event_a", time),
                        make_event(2, "event_b", time + 10),
                    ]);
                    time += 200;
                }
                2 => {
                    engine.scan(time);
                    time += 500;
                }
                3 => {
                    engine.prune(time);
                }
                _ => unreachable!(),
            }

            let snap = engine.telemetry().snapshot();
            prop_assert!(snap.events_ingested >= prev.events_ingested,
                "events_ingested decreased: {} -> {}",
                prev.events_ingested, snap.events_ingested);
            prop_assert!(snap.scans >= prev.scans,
                "scans decreased: {} -> {}", prev.scans, snap.scans);
            prop_assert!(snap.correlations_found >= prev.correlations_found,
                "correlations_found decreased: {} -> {}",
                prev.correlations_found, snap.correlations_found);
            prop_assert!(snap.prunes >= prev.prunes,
                "prunes decreased: {} -> {}", prev.prunes, snap.prunes);
            prop_assert!(snap.events_pruned >= prev.events_pruned,
                "events_pruned decreased: {} -> {}",
                prev.events_pruned, snap.events_pruned);

            prev = snap;
        }
    }

    #[test]
    fn events_pruned_bounded_by_ingested(
        n_events in 1usize..30,
    ) {
        let mut engine = CorrelationEngine::new(test_config());
        for i in 0..n_events {
            engine.ingest(make_event(i as u64, "event", 1000 + i as u64 * 100));
        }
        // Prune at a time that should expire all events
        engine.prune(1_000_000);

        let snap = engine.telemetry().snapshot();
        prop_assert!(
            snap.events_pruned <= snap.events_ingested,
            "events_pruned ({}) > events_ingested ({})",
            snap.events_pruned, snap.events_ingested,
        );
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        events_ingested in 0u64..100000,
        scans in 0u64..50000,
        correlations_found in 0u64..50000,
        prunes in 0u64..50000,
        events_pruned in 0u64..100000,
    ) {
        let snap = CorrelationEngineTelemetrySnapshot {
            events_ingested,
            scans,
            correlations_found,
            prunes,
            events_pruned,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: CorrelationEngineTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
