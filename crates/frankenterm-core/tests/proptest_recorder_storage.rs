//! Property-based tests for the `recorder_storage` module.
//!
//! Covers serde roundtrips for `RecorderBackendKind`, `DurabilityLevel`,
//! `FlushMode`, `RecorderOffset`, `AppendResponse`, `CheckpointConsumerId`,
//! `RecorderCheckpoint`, `CheckpointCommitOutcome`, `RecorderStorageHealth`,
//! `RecorderConsumerLag`, `RecorderStorageLag`, `FlushStats`, and
//! `RecorderStorageErrorClass`.

use frankenterm_core::recorder_storage::{
    AppendResponse, CheckpointCommitOutcome, CheckpointConsumerId, DurabilityLevel, FlushMode,
    FlushStats, RecorderBackendKind, RecorderCheckpoint, RecorderConsumerLag, RecorderOffset,
    RecorderStorageErrorClass, RecorderStorageHealth, RecorderStorageLag,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_backend_kind() -> impl Strategy<Value = RecorderBackendKind> {
    prop_oneof![
        Just(RecorderBackendKind::AppendLog),
        Just(RecorderBackendKind::FrankenSqlite),
    ]
}

fn arb_durability_level() -> impl Strategy<Value = DurabilityLevel> {
    prop_oneof![
        Just(DurabilityLevel::Enqueued),
        Just(DurabilityLevel::Appended),
        Just(DurabilityLevel::Fsync),
    ]
}

fn arb_flush_mode() -> impl Strategy<Value = FlushMode> {
    prop_oneof![Just(FlushMode::Buffered), Just(FlushMode::Durable),]
}

fn arb_offset() -> impl Strategy<Value = RecorderOffset> {
    (0_u64..100, 0_u64..1_000_000, 0_u64..1_000_000).prop_map(
        |(segment_id, byte_offset, ordinal)| RecorderOffset {
            segment_id,
            byte_offset,
            ordinal,
        },
    )
}

fn arb_checkpoint_consumer_id() -> impl Strategy<Value = CheckpointConsumerId> {
    "[a-z_]{3,15}".prop_map(CheckpointConsumerId)
}

fn arb_error_class() -> impl Strategy<Value = RecorderStorageErrorClass> {
    prop_oneof![
        Just(RecorderStorageErrorClass::Retryable),
        Just(RecorderStorageErrorClass::Overload),
        Just(RecorderStorageErrorClass::TerminalConfig),
        Just(RecorderStorageErrorClass::TerminalData),
        Just(RecorderStorageErrorClass::Corruption),
        Just(RecorderStorageErrorClass::DependencyUnavailable),
    ]
}

fn arb_commit_outcome() -> impl Strategy<Value = CheckpointCommitOutcome> {
    prop_oneof![
        Just(CheckpointCommitOutcome::Advanced),
        Just(CheckpointCommitOutcome::NoopAlreadyAdvanced),
        Just(CheckpointCommitOutcome::RejectedOutOfOrder),
    ]
}

// =========================================================================
// Enum serde roundtrips
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_backend_kind_serde(kind in arb_backend_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let back: RecorderBackendKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, kind);
    }

    #[test]
    fn prop_backend_kind_snake_case(kind in arb_backend_kind()) {
        let json = serde_json::to_string(&kind).unwrap();
        let expected = match kind {
            RecorderBackendKind::AppendLog => "\"append_log\"",
            RecorderBackendKind::FrankenSqlite => "\"franken_sqlite\"",
        };
        prop_assert_eq!(&json, expected);
    }

    #[test]
    fn prop_durability_serde(level in arb_durability_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let back: DurabilityLevel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, level);
    }

    #[test]
    fn prop_durability_snake_case(level in arb_durability_level()) {
        let json = serde_json::to_string(&level).unwrap();
        let expected = match level {
            DurabilityLevel::Enqueued => "\"enqueued\"",
            DurabilityLevel::Appended => "\"appended\"",
            DurabilityLevel::Fsync => "\"fsync\"",
        };
        prop_assert_eq!(&json, expected);
    }

    #[test]
    fn prop_flush_mode_serde(mode in arb_flush_mode()) {
        let json = serde_json::to_string(&mode).unwrap();
        let back: FlushMode = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, mode);
    }

    #[test]
    fn prop_commit_outcome_serde(outcome in arb_commit_outcome()) {
        let json = serde_json::to_string(&outcome).unwrap();
        let back: CheckpointCommitOutcome = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, outcome);
    }

    #[test]
    fn prop_error_class_serde(class in arb_error_class()) {
        let json = serde_json::to_string(&class).unwrap();
        let back: RecorderStorageErrorClass = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, class);
    }

    #[test]
    fn prop_error_class_snake_case(class in arb_error_class()) {
        let json = serde_json::to_string(&class).unwrap();
        let expected = match class {
            RecorderStorageErrorClass::Retryable => "\"retryable\"",
            RecorderStorageErrorClass::Overload => "\"overload\"",
            RecorderStorageErrorClass::TerminalConfig => "\"terminal_config\"",
            RecorderStorageErrorClass::TerminalData => "\"terminal_data\"",
            RecorderStorageErrorClass::Corruption => "\"corruption\"",
            RecorderStorageErrorClass::DependencyUnavailable => "\"dependency_unavailable\"",
        };
        prop_assert_eq!(&json, expected);
    }
}

// =========================================================================
// RecorderOffset — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_offset_serde(offset in arb_offset()) {
        let json = serde_json::to_string(&offset).unwrap();
        let back: RecorderOffset = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, offset);
    }

    #[test]
    fn prop_offset_deterministic(offset in arb_offset()) {
        let j1 = serde_json::to_string(&offset).unwrap();
        let j2 = serde_json::to_string(&offset).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// AppendResponse — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_append_response_serde(
        backend in arb_backend_kind(),
        accepted_count in 0_usize..1000,
        first in arb_offset(),
        last in arb_offset(),
        durability in arb_durability_level(),
        committed_at_ms in 1_000_000_000_000_u64..2_000_000_000_000,
    ) {
        let response = AppendResponse {
            backend,
            accepted_count,
            first_offset: first,
            last_offset: last,
            committed_durability: durability,
            committed_at_ms,
        };
        let json = serde_json::to_string(&response).unwrap();
        let back: AppendResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, response);
    }
}

// =========================================================================
// CheckpointConsumerId — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_consumer_id_serde(id in arb_checkpoint_consumer_id()) {
        let json = serde_json::to_string(&id).unwrap();
        let back: CheckpointConsumerId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, id);
    }
}

// =========================================================================
// RecorderCheckpoint — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_checkpoint_serde(
        consumer in arb_checkpoint_consumer_id(),
        offset in arb_offset(),
        version in "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}",
        committed_at_ms in 1_000_000_000_000_u64..2_000_000_000_000,
    ) {
        let checkpoint = RecorderCheckpoint {
            consumer,
            upto_offset: offset,
            schema_version: version,
            committed_at_ms,
        };
        let json = serde_json::to_string(&checkpoint).unwrap();
        let back: RecorderCheckpoint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, checkpoint);
    }
}

// =========================================================================
// RecorderStorageHealth — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_health_serde(
        backend in arb_backend_kind(),
        degraded in any::<bool>(),
        queue_depth in 0_usize..1000,
        queue_capacity in 0_usize..10_000,
        has_offset in any::<bool>(),
        has_error in any::<bool>(),
    ) {
        let health = RecorderStorageHealth {
            backend,
            degraded,
            queue_depth,
            queue_capacity,
            latest_offset: if has_offset {
                Some(RecorderOffset { segment_id: 0, byte_offset: 100, ordinal: 50 })
            } else {
                None
            },
            last_error: if has_error {
                Some("disk full".to_string())
            } else {
                None
            },
        };
        let json = serde_json::to_string(&health).unwrap();
        let back: RecorderStorageHealth = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, health);
    }
}

// =========================================================================
// RecorderConsumerLag + RecorderStorageLag — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_consumer_lag_serde(
        consumer in arb_checkpoint_consumer_id(),
        offsets_behind in 0_u64..100_000,
    ) {
        let lag = RecorderConsumerLag {
            consumer,
            offsets_behind,
        };
        let json = serde_json::to_string(&lag).unwrap();
        let back: RecorderConsumerLag = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, lag);
    }

    #[test]
    fn prop_storage_lag_serde(
        n in 0_usize..5,
        has_offset in any::<bool>(),
    ) {
        let consumers: Vec<RecorderConsumerLag> = (0..n)
            .map(|i| RecorderConsumerLag {
                consumer: CheckpointConsumerId(format!("consumer_{i}")),
                offsets_behind: i as u64 * 100,
            })
            .collect();
        let lag = RecorderStorageLag {
            latest_offset: if has_offset {
                Some(RecorderOffset { segment_id: 0, byte_offset: 500, ordinal: 200 })
            } else {
                None
            },
            consumers,
        };
        let json = serde_json::to_string(&lag).unwrap();
        let back: RecorderStorageLag = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, lag);
    }
}

// =========================================================================
// FlushStats — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_flush_stats_serde(
        backend in arb_backend_kind(),
        flushed_at_ms in 1_000_000_000_000_u64..2_000_000_000_000,
        has_offset in any::<bool>(),
    ) {
        let stats = FlushStats {
            backend,
            flushed_at_ms,
            latest_offset: if has_offset {
                Some(RecorderOffset { segment_id: 0, byte_offset: 1000, ordinal: 500 })
            } else {
                None
            },
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: FlushStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, stats);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn all_backend_kinds_distinct_json() {
    let a = serde_json::to_string(&RecorderBackendKind::AppendLog).unwrap();
    let b = serde_json::to_string(&RecorderBackendKind::FrankenSqlite).unwrap();
    assert_ne!(a, b);
}

#[test]
fn all_durability_levels_distinct_json() {
    let levels = [
        DurabilityLevel::Enqueued,
        DurabilityLevel::Appended,
        DurabilityLevel::Fsync,
    ];
    let jsons: Vec<_> = levels
        .iter()
        .map(|l| serde_json::to_string(l).unwrap())
        .collect();
    for i in 0..jsons.len() {
        for j in (i + 1)..jsons.len() {
            assert_ne!(jsons[i], jsons[j]);
        }
    }
}

#[test]
fn all_error_classes_distinct_json() {
    let classes = [
        RecorderStorageErrorClass::Retryable,
        RecorderStorageErrorClass::Overload,
        RecorderStorageErrorClass::TerminalConfig,
        RecorderStorageErrorClass::TerminalData,
        RecorderStorageErrorClass::Corruption,
        RecorderStorageErrorClass::DependencyUnavailable,
    ];
    let jsons: Vec<_> = classes
        .iter()
        .map(|c| serde_json::to_string(c).unwrap())
        .collect();
    for i in 0..jsons.len() {
        for j in (i + 1)..jsons.len() {
            assert_ne!(jsons[i], jsons[j]);
        }
    }
}
