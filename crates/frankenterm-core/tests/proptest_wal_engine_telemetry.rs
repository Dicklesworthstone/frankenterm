//! Property-based tests for WAL engine telemetry counters (ft-3kxe.16).
//!
//! Validates:
//! 1. Telemetry starts at zero
//! 2. appends tracks append() calls
//! 3. checkpoints tracks checkpoint() calls
//! 4. compactions / entries_compacted track compact() effects
//! 5. truncations / entries_truncated track truncate_after() effects
//! 6. Serde roundtrip for snapshot
//! 7. Counter monotonicity across operations

use proptest::prelude::*;

use frankenterm_core::wal_engine::{WalConfig, WalEngine, WalTelemetrySnapshot};

// =============================================================================
// Helpers
// =============================================================================

fn make_engine() -> WalEngine<String> {
    WalEngine::new(WalConfig {
        compaction_threshold: 100,
        max_retained_entries: 50,
    })
}

fn make_engine_small() -> WalEngine<String> {
    WalEngine::new(WalConfig {
        compaction_threshold: 5,
        max_retained_entries: 3,
    })
}

// =============================================================================
// Unit tests
// =============================================================================

#[test]
fn telemetry_starts_at_zero() {
    let engine = make_engine();
    let snap = engine.telemetry().snapshot();

    assert_eq!(snap.appends, 0);
    assert_eq!(snap.checkpoints, 0);
    assert_eq!(snap.compactions, 0);
    assert_eq!(snap.entries_compacted, 0);
    assert_eq!(snap.truncations, 0);
    assert_eq!(snap.entries_truncated, 0);
}

#[test]
fn appends_tracked() {
    let mut engine = make_engine();

    engine.append("a".to_string(), 1000);
    engine.append("b".to_string(), 2000);
    engine.append("c".to_string(), 3000);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.appends, 3);
}

#[test]
fn checkpoints_tracked() {
    let mut engine = make_engine();

    engine.append("a".to_string(), 1000);
    engine.checkpoint(2000);
    engine.append("b".to_string(), 3000);
    engine.checkpoint(4000);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.checkpoints, 2);
    assert_eq!(snap.appends, 2);
}

#[test]
fn compaction_tracked() {
    let mut engine = make_engine();

    // Add entries, checkpoint, then compact
    for i in 0..10 {
        engine.append(format!("entry-{i}"), i * 1000);
    }
    engine.checkpoint(10_000);

    let removed = engine.compact();
    assert!(removed > 0);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.compactions, 1);
    assert_eq!(snap.entries_compacted, removed as u64);
}

#[test]
fn compaction_noop_when_nothing_to_remove() {
    let mut engine = make_engine();

    // Just a checkpoint, nothing before it to compact
    engine.checkpoint(1000);
    let removed = engine.compact();
    assert_eq!(removed, 0);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.compactions, 0); // no-op doesn't count
}

#[test]
fn truncation_tracked() {
    let mut engine = make_engine();

    let seq1 = engine.append("a".to_string(), 1000);
    engine.append("b".to_string(), 2000);
    engine.append("c".to_string(), 3000);

    engine.truncate_after(seq1);

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.truncations, 1);
    assert_eq!(snap.entries_truncated, 2); // b and c removed
}

#[test]
fn truncation_noop_when_nothing_removed() {
    let mut engine = make_engine();

    let seq = engine.append("a".to_string(), 1000);
    engine.truncate_after(seq); // nothing after seq

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.truncations, 0); // no-op
}

#[test]
fn multiple_compactions_accumulate() {
    let mut engine = make_engine_small();

    for i in 0..5 {
        engine.append(format!("e{i}"), i * 1000);
    }
    engine.checkpoint(5000);
    let r1 = engine.compact();

    for i in 5..10 {
        engine.append(format!("e{i}"), i * 1000);
    }
    engine.checkpoint(10_000);
    let r2 = engine.compact();

    let snap = engine.telemetry().snapshot();
    assert_eq!(snap.compactions, 2);
    assert_eq!(snap.entries_compacted, (r1 + r2) as u64);
}

#[test]
fn snapshot_serde_roundtrip() {
    let snap = WalTelemetrySnapshot {
        appends: 100,
        checkpoints: 10,
        compactions: 5,
        entries_compacted: 80,
        truncations: 2,
        entries_truncated: 15,
    };
    let json = serde_json::to_string(&snap).expect("serialize");
    let back: WalTelemetrySnapshot = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(snap, back);
}

#[test]
fn mixed_operations() {
    let mut engine = make_engine();

    // Appends
    let _s1 = engine.append("a".to_string(), 1000);
    let _s2 = engine.append("b".to_string(), 2000);
    let s3 = engine.append("c".to_string(), 3000);

    // Checkpoint
    engine.checkpoint(4000);

    // More appends
    engine.append("d".to_string(), 5000);

    // Truncate back
    engine.truncate_after(s3);

    // Compact (checkpoint at s3+1 was truncated, but entries before are still there)
    engine.checkpoint(6000);
    for i in 0..5 {
        engine.append(format!("e{i}"), 7000 + i * 1000);
    }
    engine.checkpoint(12_000);
    engine.compact();

    let snap = engine.telemetry().snapshot();
    assert!(snap.appends >= 4);
    assert!(snap.checkpoints >= 1);
    // At least one truncation removed entries
    assert_eq!(snap.truncations, 1);
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    #[test]
    fn appends_equals_call_count(
        count in 1usize..100,
    ) {
        let mut engine = make_engine();
        for i in 0..count {
            engine.append(format!("entry-{i}"), i as u64 * 1000);
        }
        let snap = engine.telemetry().snapshot();
        prop_assert_eq!(snap.appends, count as u64);
    }

    #[test]
    fn checkpoints_equals_call_count(
        append_count in 1usize..20,
        checkpoint_count in 1usize..10,
    ) {
        let mut engine = make_engine();
        for i in 0..append_count {
            engine.append(format!("e{i}"), i as u64 * 1000);
        }
        for i in 0..checkpoint_count {
            engine.checkpoint((append_count + i) as u64 * 1000);
        }
        let snap = engine.telemetry().snapshot();
        prop_assert_eq!(snap.checkpoints, checkpoint_count as u64);
        prop_assert_eq!(snap.appends, append_count as u64);
    }

    #[test]
    fn counters_monotonically_increase(
        ops in prop::collection::vec(
            prop_oneof![
                Just(0u8), // append
                Just(1u8), // checkpoint
                Just(2u8), // compact
            ],
            1..30,
        ),
    ) {
        let mut engine = make_engine();
        let mut prev = engine.telemetry().snapshot();

        for (i, op) in ops.iter().enumerate() {
            match op {
                0 => { engine.append(format!("e{i}"), i as u64 * 1000); }
                1 => { engine.checkpoint(i as u64 * 1000); }
                2 => { engine.compact(); }
                _ => unreachable!(),
            }

            let snap = engine.telemetry().snapshot();
            prop_assert!(snap.appends >= prev.appends,
                "appends decreased: {} -> {}", prev.appends, snap.appends);
            prop_assert!(snap.checkpoints >= prev.checkpoints,
                "checkpoints decreased: {} -> {}", prev.checkpoints, snap.checkpoints);
            prop_assert!(snap.compactions >= prev.compactions,
                "compactions decreased: {} -> {}", prev.compactions, snap.compactions);
            prop_assert!(snap.entries_compacted >= prev.entries_compacted,
                "entries_compacted decreased: {} -> {}", prev.entries_compacted, snap.entries_compacted);

            prev = snap;
        }
    }

    #[test]
    fn entries_compacted_bounded_by_appends_plus_checkpoints(
        append_count in 5usize..30,
    ) {
        let mut engine = make_engine_small();
        for i in 0..append_count {
            engine.append(format!("e{i}"), i as u64 * 1000);
        }
        engine.checkpoint(append_count as u64 * 1000);
        engine.compact();

        let snap = engine.telemetry().snapshot();
        // Can't compact more entries than we've written (appends + checkpoints)
        prop_assert!(
            snap.entries_compacted <= snap.appends + snap.checkpoints,
            "compacted {} > appends {} + checkpoints {}",
            snap.entries_compacted, snap.appends, snap.checkpoints
        );
    }

    #[test]
    fn snapshot_roundtrip_arbitrary(
        appends in 0u64..100000,
        checkpoints in 0u64..10000,
        compactions in 0u64..5000,
        entries_comp in 0u64..100000,
        truncations in 0u64..5000,
        entries_trunc in 0u64..100000,
    ) {
        let snap = WalTelemetrySnapshot {
            appends,
            checkpoints,
            compactions,
            entries_compacted: entries_comp,
            truncations,
            entries_truncated: entries_trunc,
        };

        let json = serde_json::to_string(&snap).expect("serialize");
        let back: WalTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(snap, back);
    }
}
