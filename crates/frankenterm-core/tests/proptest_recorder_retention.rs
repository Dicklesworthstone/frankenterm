//! Property-based tests for recorder_retention carrier types.
//!
//! Covers serde roundtrips and basic aggregate invariants for retention-facing
//! configuration, sweep results, stats, and audit events.

use frankenterm_core::recorder_retention::{
    RetentionAuditEvent, RetentionAuditType, RetentionConfig, RetentionStats, RetentionSweepResult,
    SegmentPhase, SensitivityTier,
};
use proptest::prelude::*;

fn arb_sensitivity_tier() -> impl Strategy<Value = SensitivityTier> {
    prop_oneof![
        Just(SensitivityTier::T1Standard),
        Just(SensitivityTier::T2Sensitive),
        Just(SensitivityTier::T3Restricted),
    ]
}

fn arb_segment_phase() -> impl Strategy<Value = SegmentPhase> {
    prop_oneof![
        Just(SegmentPhase::Active),
        Just(SegmentPhase::Sealed),
        Just(SegmentPhase::Archived),
        Just(SegmentPhase::Purged),
    ]
}

fn arb_audit_type() -> impl Strategy<Value = RetentionAuditType> {
    prop_oneof![
        Just(RetentionAuditType::SegmentSealed),
        Just(RetentionAuditType::SegmentArchived),
        Just(RetentionAuditType::SegmentPurged),
        Just(RetentionAuditType::AcceleratedPurge),
        Just(RetentionAuditType::ManualPurge),
        Just(RetentionAuditType::PolicyOverride),
    ]
}

fn arb_segment_id() -> impl Strategy<Value = String> {
    "[a-z0-9_-]{4,24}".prop_map(|s| s.to_string())
}

fn arb_retention_config() -> impl Strategy<Value = RetentionConfig> {
    (
        1u32..=168,
        1u32..=90,
        1u32..=365,
        1u32..=168,
        0u32..=90,
        1u64..=(1024 * 1024 * 1024),
        1u64..=(24 * 3600),
    )
        .prop_map(
            |(
                hot_hours,
                warm_days,
                cold_days,
                t3_max_hours,
                t1_extended_days,
                max_segment_bytes,
                max_segment_duration_secs,
            )| RetentionConfig {
                hot_hours,
                warm_days,
                cold_days,
                t3_max_hours,
                t1_extended_days,
                max_segment_bytes,
                max_segment_duration_secs,
            },
        )
}

fn arb_retention_sweep_result() -> impl Strategy<Value = RetentionSweepResult> {
    (
        prop::collection::vec(arb_segment_id(), 0..6),
        prop::collection::vec(arb_segment_id(), 0..6),
        prop::collection::vec(arb_segment_id(), 0..6),
        prop::collection::vec(arb_segment_id(), 0..6),
        prop::collection::vec((arb_segment_id(), "[a-z0-9_-]{3,20}"), 0..6),
    )
        .prop_map(|(sealed, archived, purge_candidates, purged, held)| RetentionSweepResult {
            sealed,
            archived,
            purge_candidates,
            purged,
            held,
        })
}

fn arb_retention_stats() -> impl Strategy<Value = RetentionStats> {
    (
        0usize..50,
        0u64..1_000_000,
        0usize..50,
        0u64..1_000_000,
        0usize..50,
        0u64..1_000_000,
        0usize..50,
    )
        .prop_map(
            |(
                active_count,
                active_bytes,
                sealed_count,
                sealed_bytes,
                archived_count,
                archived_bytes,
                purged_count,
            )| RetentionStats {
                active_count,
                active_bytes,
                sealed_count,
                sealed_bytes,
                archived_count,
                archived_bytes,
                purged_count,
            },
        )
}

fn arb_retention_audit_event() -> impl Strategy<Value = RetentionAuditEvent> {
    (
        arb_audit_type(),
        arb_segment_id(),
        prop::option::of((0u64..1_000_000, 0u64..1_000_000).prop_map(|(a, b)| (a.min(b), a.max(b)))),
        arb_sensitivity_tier(),
        prop::option::of(arb_segment_phase()),
        arb_segment_phase(),
        0u64..u64::MAX / 2,
        prop::option::of("[a-zA-Z0-9 _./:-]{3,40}"),
    )
        .prop_map(
            |(
                event_type,
                segment_id,
                ordinal_range,
                sensitivity,
                from_phase,
                to_phase,
                timestamp_ms,
                justification,
            )| RetentionAuditEvent {
                audit_version: "v1".to_string(),
                event_type,
                segment_id,
                ordinal_range,
                sensitivity,
                from_phase,
                to_phase,
                timestamp_ms,
                justification,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn retention_config_serde_roundtrip_and_validate(config in arb_retention_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: RetentionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.hot_hours, config.hot_hours);
        prop_assert_eq!(back.warm_days, config.warm_days);
        prop_assert_eq!(back.cold_days, config.cold_days);
        prop_assert_eq!(back.t3_max_hours, config.t3_max_hours);
        prop_assert_eq!(back.t1_extended_days, config.t1_extended_days);
        prop_assert_eq!(back.max_segment_bytes, config.max_segment_bytes);
        prop_assert_eq!(back.max_segment_duration_secs, config.max_segment_duration_secs);
        prop_assert!(back.validate().is_ok());
    }

    #[test]
    fn retention_sweep_result_serde_roundtrip(result in arb_retention_sweep_result()) {
        let json = serde_json::to_string(&result).unwrap();
        let back: RetentionSweepResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.sealed, result.sealed);
        prop_assert_eq!(back.archived, result.archived);
        prop_assert_eq!(back.purge_candidates, result.purge_candidates);
        prop_assert_eq!(back.purged, result.purged);
        prop_assert_eq!(back.held, result.held);
    }

    #[test]
    fn retention_stats_live_totals_match_components(stats in arb_retention_stats()) {
        prop_assert_eq!(
            stats.live_count(),
            stats.active_count + stats.sealed_count + stats.archived_count
        );
        prop_assert_eq!(
            stats.live_bytes(),
            stats.active_bytes + stats.sealed_bytes + stats.archived_bytes
        );
    }

    #[test]
    fn retention_audit_event_serde_roundtrip(event in arb_retention_audit_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let back: RetentionAuditEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.audit_version, "v1");
        prop_assert_eq!(back.event_type, event.event_type);
        prop_assert_eq!(back.segment_id, event.segment_id);
        prop_assert_eq!(back.ordinal_range, event.ordinal_range);
        prop_assert_eq!(back.sensitivity, event.sensitivity);
        prop_assert_eq!(back.from_phase, event.from_phase);
        prop_assert_eq!(back.to_phase, event.to_phase);
        prop_assert_eq!(back.timestamp_ms, event.timestamp_ms);
        prop_assert_eq!(back.justification, event.justification);
    }
}
