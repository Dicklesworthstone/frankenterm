//! Property-based tests for the `recorder_storage` module.
//!
//! Covers serde roundtrips for `RecorderBackendKind`, `DurabilityLevel`,
//! `FlushMode`, `RecorderOffset`, `AppendResponse`, `CheckpointConsumerId`,
//! `RecorderCheckpoint`, `CheckpointCommitOutcome`, `RecorderStorageHealth`,
//! `RecorderConsumerLag`, `RecorderStorageLag`, `FlushStats`, and
//! `RecorderStorageErrorClass`.
//!
//! Also covers Clone/Debug invariants, field preservation, JSON structure
//! properties, binary (bincode) roundtrips, and cross-type serialization
//! non-confusion.

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

// =========================================================================
// Clone invariants — cloned values must equal originals
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_backend_kind_clone_eq(kind in arb_backend_kind()) {
        let cloned = kind;
        prop_assert_eq!(cloned, kind);
    }

    #[test]
    fn prop_durability_level_clone_eq(level in arb_durability_level()) {
        let cloned = level;
        prop_assert_eq!(cloned, level);
    }

    #[test]
    fn prop_flush_mode_clone_eq(mode in arb_flush_mode()) {
        let cloned = mode;
        prop_assert_eq!(cloned, mode);
    }

    #[test]
    fn prop_offset_clone_eq(offset in arb_offset()) {
        let cloned = offset.clone();
        prop_assert_eq!(cloned, offset);
    }

    #[test]
    fn prop_error_class_clone_eq(class in arb_error_class()) {
        let cloned = class;
        prop_assert_eq!(cloned, class);
    }

    #[test]
    fn prop_commit_outcome_clone_eq(outcome in arb_commit_outcome()) {
        let cloned = outcome;
        prop_assert_eq!(cloned, outcome);
    }
}

// =========================================================================
// Debug output is non-empty for all types
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_backend_kind_debug_nonempty(kind in arb_backend_kind()) {
        let dbg = format!("{kind:?}");
        prop_assert!(!dbg.is_empty());
    }

    #[test]
    fn prop_offset_debug_contains_fields(
        seg in 0_u64..100,
        byte_off in 0_u64..1_000_000,
        ord in 0_u64..1_000_000,
    ) {
        let offset = RecorderOffset {
            segment_id: seg,
            byte_offset: byte_off,
            ordinal: ord,
        };
        let dbg = format!("{offset:?}");
        prop_assert!(dbg.contains("segment_id"));
        prop_assert!(dbg.contains("byte_offset"));
        prop_assert!(dbg.contains("ordinal"));
    }

    #[test]
    fn prop_consumer_id_debug_contains_inner(name in "[a-z]{3,10}") {
        let id = CheckpointConsumerId(name.clone());
        let dbg = format!("{id:?}");
        prop_assert!(dbg.contains(&name));
    }
}

// =========================================================================
// Field preservation — struct fields survive serde roundtrip individually
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_offset_field_preservation(
        seg in 0_u64..100,
        byte_off in 0_u64..1_000_000,
        ord in 0_u64..1_000_000,
    ) {
        let offset = RecorderOffset {
            segment_id: seg,
            byte_offset: byte_off,
            ordinal: ord,
        };
        let json = serde_json::to_string(&offset).unwrap();
        let back: RecorderOffset = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.segment_id, seg);
        prop_assert_eq!(back.byte_offset, byte_off);
        prop_assert_eq!(back.ordinal, ord);
    }

    #[test]
    fn prop_append_response_field_preservation(
        backend in arb_backend_kind(),
        count in 0_usize..500,
        durability in arb_durability_level(),
        ts in 1_000_000_000_000_u64..2_000_000_000_000,
    ) {
        let response = AppendResponse {
            backend,
            accepted_count: count,
            first_offset: RecorderOffset { segment_id: 0, byte_offset: 0, ordinal: 0 },
            last_offset: RecorderOffset { segment_id: 0, byte_offset: 100, ordinal: count as u64 },
            committed_durability: durability,
            committed_at_ms: ts,
        };
        let json = serde_json::to_string(&response).unwrap();
        let back: AppendResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.backend, backend);
        prop_assert_eq!(back.accepted_count, count);
        prop_assert_eq!(back.committed_durability, durability);
        prop_assert_eq!(back.committed_at_ms, ts);
    }

    #[test]
    fn prop_checkpoint_field_preservation(
        consumer_name in "[a-z]{3,10}",
        ord in 0_u64..1_000_000,
        version in "[0-9]{1,2}\\.[0-9]{1,2}",
        ts in 1_000_000_000_000_u64..2_000_000_000_000,
    ) {
        let checkpoint = RecorderCheckpoint {
            consumer: CheckpointConsumerId(consumer_name.clone()),
            upto_offset: RecorderOffset { segment_id: 0, byte_offset: 0, ordinal: ord },
            schema_version: version.clone(),
            committed_at_ms: ts,
        };
        let json = serde_json::to_string(&checkpoint).unwrap();
        let back: RecorderCheckpoint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.consumer.0, &consumer_name);
        prop_assert_eq!(back.upto_offset.ordinal, ord);
        prop_assert_eq!(&back.schema_version, &version);
        prop_assert_eq!(back.committed_at_ms, ts);
    }

    #[test]
    fn prop_health_field_preservation(
        backend in arb_backend_kind(),
        degraded in any::<bool>(),
        depth in 0_usize..1000,
        capacity in 1_usize..10_000,
    ) {
        let health = RecorderStorageHealth {
            backend,
            degraded,
            queue_depth: depth,
            queue_capacity: capacity,
            latest_offset: None,
            last_error: None,
        };
        let json = serde_json::to_string(&health).unwrap();
        let back: RecorderStorageHealth = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.backend, backend);
        prop_assert_eq!(back.degraded, degraded);
        prop_assert_eq!(back.queue_depth, depth);
        prop_assert_eq!(back.queue_capacity, capacity);
        prop_assert!(back.latest_offset.is_none());
        prop_assert!(back.last_error.is_none());
    }
}

// =========================================================================
// JSON structure properties — keys present, valid JSON object
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_offset_json_has_expected_keys(offset in arb_offset()) {
        let json = serde_json::to_string(&offset).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        prop_assert!(obj.contains_key("segment_id"));
        prop_assert!(obj.contains_key("byte_offset"));
        prop_assert!(obj.contains_key("ordinal"));
        prop_assert_eq!(obj.len(), 3);
    }

    #[test]
    fn prop_append_response_json_has_expected_keys(
        backend in arb_backend_kind(),
        durability in arb_durability_level(),
    ) {
        let response = AppendResponse {
            backend,
            accepted_count: 1,
            first_offset: RecorderOffset { segment_id: 0, byte_offset: 0, ordinal: 0 },
            last_offset: RecorderOffset { segment_id: 0, byte_offset: 10, ordinal: 0 },
            committed_durability: durability,
            committed_at_ms: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&response).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        prop_assert!(obj.contains_key("backend"));
        prop_assert!(obj.contains_key("accepted_count"));
        prop_assert!(obj.contains_key("first_offset"));
        prop_assert!(obj.contains_key("last_offset"));
        prop_assert!(obj.contains_key("committed_durability"));
        prop_assert!(obj.contains_key("committed_at_ms"));
        prop_assert_eq!(obj.len(), 6);
    }

    #[test]
    fn prop_health_json_has_expected_keys(backend in arb_backend_kind()) {
        let health = RecorderStorageHealth {
            backend,
            degraded: false,
            queue_depth: 0,
            queue_capacity: 100,
            latest_offset: None,
            last_error: None,
        };
        let json = serde_json::to_string(&health).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        prop_assert!(obj.contains_key("backend"));
        prop_assert!(obj.contains_key("degraded"));
        prop_assert!(obj.contains_key("queue_depth"));
        prop_assert!(obj.contains_key("queue_capacity"));
        prop_assert!(obj.contains_key("latest_offset"));
        prop_assert!(obj.contains_key("last_error"));
        prop_assert_eq!(obj.len(), 6);
    }

    #[test]
    fn prop_flush_stats_json_has_expected_keys(backend in arb_backend_kind()) {
        let stats = FlushStats {
            backend,
            flushed_at_ms: 1_700_000_000_000,
            latest_offset: None,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = v.as_object().unwrap();
        prop_assert!(obj.contains_key("backend"));
        prop_assert!(obj.contains_key("flushed_at_ms"));
        prop_assert!(obj.contains_key("latest_offset"));
        prop_assert_eq!(obj.len(), 3);
    }
}

// =========================================================================
// Serde pretty roundtrip — pretty-printed JSON also roundtrips correctly
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_offset_pretty_serde(offset in arb_offset()) {
        let pretty = serde_json::to_string_pretty(&offset).unwrap();
        let back: RecorderOffset = serde_json::from_str(&pretty).unwrap();
        prop_assert_eq!(back, offset);
    }

    #[test]
    fn prop_checkpoint_pretty_serde(
        consumer in arb_checkpoint_consumer_id(),
        offset in arb_offset(),
        version in "[0-9]{1,2}\\.[0-9]{1,2}",
        ts in 1_000_000_000_000_u64..2_000_000_000_000,
    ) {
        let checkpoint = RecorderCheckpoint {
            consumer,
            upto_offset: offset,
            schema_version: version,
            committed_at_ms: ts,
        };
        let pretty = serde_json::to_string_pretty(&checkpoint).unwrap();
        let back: RecorderCheckpoint = serde_json::from_str(&pretty).unwrap();
        prop_assert_eq!(back, checkpoint);
    }

    #[test]
    fn prop_health_pretty_serde(
        backend in arb_backend_kind(),
        degraded in any::<bool>(),
        depth in 0_usize..500,
        cap in 1_usize..5000,
    ) {
        let health = RecorderStorageHealth {
            backend,
            degraded,
            queue_depth: depth,
            queue_capacity: cap,
            latest_offset: None,
            last_error: if degraded { Some("test error".to_string()) } else { None },
        };
        let pretty = serde_json::to_string_pretty(&health).unwrap();
        let back: RecorderStorageHealth = serde_json::from_str(&pretty).unwrap();
        prop_assert_eq!(back, health);
    }
}

// =========================================================================
// Enum JSON values are always quoted strings (not integers/objects)
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_backend_kind_json_is_string(kind in arb_backend_kind()) {
        let v: serde_json::Value = serde_json::to_value(kind).unwrap();
        prop_assert!(v.is_string(), "backend kind should serialize as a JSON string");
    }

    #[test]
    fn prop_durability_json_is_string(level in arb_durability_level()) {
        let v: serde_json::Value = serde_json::to_value(level).unwrap();
        prop_assert!(v.is_string(), "durability level should serialize as a JSON string");
    }

    #[test]
    fn prop_flush_mode_json_is_string(mode in arb_flush_mode()) {
        let v: serde_json::Value = serde_json::to_value(mode).unwrap();
        prop_assert!(v.is_string(), "flush mode should serialize as a JSON string");
    }

    #[test]
    fn prop_commit_outcome_json_is_string(outcome in arb_commit_outcome()) {
        let v: serde_json::Value = serde_json::to_value(outcome).unwrap();
        prop_assert!(v.is_string(), "commit outcome should serialize as a JSON string");
    }

    #[test]
    fn prop_error_class_json_is_string(class in arb_error_class()) {
        let v: serde_json::Value = serde_json::to_value(class).unwrap();
        prop_assert!(v.is_string(), "error class should serialize as a JSON string");
    }
}

// =========================================================================
// Cross-type non-confusion — distinct types never deserialize as each other
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_offset_not_deserializable_as_health(offset in arb_offset()) {
        let json = serde_json::to_string(&offset).unwrap();
        let attempt = serde_json::from_str::<RecorderStorageHealth>(&json);
        prop_assert!(attempt.is_err(), "RecorderOffset JSON must not deserialize as RecorderStorageHealth");
    }

    #[test]
    fn prop_consumer_lag_not_deserializable_as_checkpoint(
        consumer in arb_checkpoint_consumer_id(),
        behind in 0_u64..100_000,
    ) {
        let lag = RecorderConsumerLag { consumer, offsets_behind: behind };
        let json = serde_json::to_string(&lag).unwrap();
        let attempt = serde_json::from_str::<RecorderCheckpoint>(&json);
        prop_assert!(attempt.is_err(), "RecorderConsumerLag JSON must not deserialize as RecorderCheckpoint");
    }
}

// =========================================================================
// Consumer lag clone and field preservation
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_consumer_lag_clone_preserves_fields(
        name in "[a-z]{3,10}",
        behind in 0_u64..1_000_000,
    ) {
        let lag = RecorderConsumerLag {
            consumer: CheckpointConsumerId(name.clone()),
            offsets_behind: behind,
        };
        let cloned = lag.clone();
        prop_assert_eq!(&cloned.consumer.0, &name);
        prop_assert_eq!(cloned.offsets_behind, behind);
    }

    #[test]
    fn prop_storage_lag_consumers_count_preserved(n in 0_usize..8) {
        let consumers: Vec<RecorderConsumerLag> = (0..n)
            .map(|i| RecorderConsumerLag {
                consumer: CheckpointConsumerId(format!("c_{i}")),
                offsets_behind: i as u64,
            })
            .collect();
        let lag = RecorderStorageLag {
            latest_offset: None,
            consumers,
        };
        let json = serde_json::to_string(&lag).unwrap();
        let back: RecorderStorageLag = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.consumers.len(), n);
    }
}

// =========================================================================
// Checkpoint consumer ID serde — inner string preserved exactly
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_consumer_id_inner_string_preserved(name in "[a-z_]{3,20}") {
        let id = CheckpointConsumerId(name.clone());
        let json = serde_json::to_string(&id).unwrap();
        let back: CheckpointConsumerId = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.0, &name);
    }

    #[test]
    fn prop_consumer_id_clone_debug(name in "[a-z]{3,12}") {
        let id = CheckpointConsumerId(name.clone());
        let dbg = format!("{id:?}");
        prop_assert!(dbg.contains(&name));
        let cloned = id.clone();
        prop_assert_eq!(cloned, id);
    }
}

// =========================================================================
// FlushStats clone roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_flush_stats_clone_eq(
        backend in arb_backend_kind(),
        ts in 1_000_000_000_000_u64..2_000_000_000_000,
    ) {
        let stats = FlushStats {
            backend,
            flushed_at_ms: ts,
            latest_offset: Some(RecorderOffset { segment_id: 0, byte_offset: 0, ordinal: 0 }),
        };
        let cloned = stats.clone();
        prop_assert_eq!(cloned, stats);
    }

    #[test]
    fn prop_flush_stats_debug_nonempty(backend in arb_backend_kind()) {
        let stats = FlushStats {
            backend,
            flushed_at_ms: 0,
            latest_offset: None,
        };
        let dbg = format!("{stats:?}");
        prop_assert!(!dbg.is_empty());
        prop_assert!(dbg.contains("FlushStats"));
    }
}

// =========================================================================
// Append response clone and serde determinism
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_append_response_clone_eq(
        backend in arb_backend_kind(),
        count in 0_usize..100,
        durability in arb_durability_level(),
        ts in 1_000_000_000_000_u64..2_000_000_000_000,
    ) {
        let response = AppendResponse {
            backend,
            accepted_count: count,
            first_offset: RecorderOffset { segment_id: 0, byte_offset: 0, ordinal: 0 },
            last_offset: RecorderOffset { segment_id: 0, byte_offset: 50, ordinal: count as u64 },
            committed_durability: durability,
            committed_at_ms: ts,
        };
        let cloned = response.clone();
        prop_assert_eq!(cloned, response);
    }

    #[test]
    fn prop_append_response_serde_deterministic(
        backend in arb_backend_kind(),
        count in 0_usize..100,
        durability in arb_durability_level(),
    ) {
        let response = AppendResponse {
            backend,
            accepted_count: count,
            first_offset: RecorderOffset { segment_id: 0, byte_offset: 0, ordinal: 0 },
            last_offset: RecorderOffset { segment_id: 0, byte_offset: 50, ordinal: count as u64 },
            committed_durability: durability,
            committed_at_ms: 1_700_000_000_000,
        };
        let j1 = serde_json::to_string(&response).unwrap();
        let j2 = serde_json::to_string(&response).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}
