//! Property-based tests for recorder retention, partitioning, and archival lifecycle.
//!
//! Verifies invariants across the full surface of `recorder_retention`:
//! - RetentionConfig validation and serialization
//! - SensitivityTier classification and ordering
//! - SegmentPhase lifecycle constraints
//! - SegmentMeta rolling, transitions, and timing
//! - RetentionManager operations and sweep correctness
//! - Audit types and error formatting

use std::collections::HashMap;

use proptest::prelude::*;

use frankenterm_core::recorder_retention::{
    RetentionAuditEvent, RetentionAuditType, RetentionConfig, RetentionError, RetentionManager,
    RetentionStats, RetentionSweepResult, SegmentMeta, SegmentPhase, SensitivityTier,
};
use frankenterm_core::recording::RecorderRedactionLevel;

// =============================================================================
// Helper constants
// =============================================================================

const MS_PER_HOUR: u64 = 3_600_000;
const MS_PER_DAY: u64 = 86_400_000;

// =============================================================================
// Proptest strategies
// =============================================================================

/// Generate a valid RetentionConfig (all fields nonzero, t1_extended_days <= 90).
fn arb_valid_config() -> impl Strategy<Value = RetentionConfig> {
    (
        1u32..=720,          // hot_hours
        1u32..=365,          // warm_days
        1u32..=365,          // cold_days
        1u32..=720,          // t3_max_hours
        1u32..=90,           // t1_extended_days
        1u64..=1_073_741_824, // max_segment_bytes (up to 1GB)
        1u64..=86_400,       // max_segment_duration_secs (up to 1 day)
    )
        .prop_map(
            |(hot_hours, warm_days, cold_days, t3_max_hours, t1_extended_days, max_segment_bytes, max_segment_duration_secs)| {
                RetentionConfig {
                    hot_hours,
                    warm_days,
                    cold_days,
                    t3_max_hours,
                    t1_extended_days,
                    max_segment_bytes,
                    max_segment_duration_secs,
                }
            },
        )
}

/// Generate an arbitrary SensitivityTier.
fn arb_tier() -> impl Strategy<Value = SensitivityTier> {
    prop_oneof![
        Just(SensitivityTier::T1Standard),
        Just(SensitivityTier::T2Sensitive),
        Just(SensitivityTier::T3Restricted),
    ]
}

/// Generate an arbitrary SegmentPhase.
fn arb_phase() -> impl Strategy<Value = SegmentPhase> {
    prop_oneof![
        Just(SegmentPhase::Active),
        Just(SegmentPhase::Sealed),
        Just(SegmentPhase::Archived),
        Just(SegmentPhase::Purged),
    ]
}

/// Generate an arbitrary RecorderRedactionLevel.
fn arb_redaction() -> impl Strategy<Value = RecorderRedactionLevel> {
    prop_oneof![
        Just(RecorderRedactionLevel::None),
        Just(RecorderRedactionLevel::Partial),
        Just(RecorderRedactionLevel::Full),
    ]
}

/// Generate an arbitrary RetentionAuditType.
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

/// Build a SegmentMeta in the Active phase with given parameters.
fn make_active_segment(
    id: &str,
    tier: SensitivityTier,
    created_at_ms: u64,
    size_bytes: u64,
    event_count: u64,
) -> SegmentMeta {
    SegmentMeta {
        segment_id: id.to_string(),
        sensitivity: tier,
        phase: SegmentPhase::Active,
        start_ordinal: 0,
        end_ordinal: None,
        size_bytes,
        created_at_ms,
        sealed_at_ms: None,
        archived_at_ms: None,
        purged_at_ms: None,
        event_count,
    }
}

/// Build a SegmentMeta in a given phase with appropriate timestamps.
fn make_segment_at_phase(
    id: &str,
    tier: SensitivityTier,
    phase: SegmentPhase,
    created_at_ms: u64,
) -> SegmentMeta {
    let sealed_at_ms = if phase >= SegmentPhase::Sealed {
        Some(created_at_ms + 24 * MS_PER_HOUR)
    } else {
        None
    };
    let archived_at_ms = if phase >= SegmentPhase::Archived {
        Some(created_at_ms + 24 * MS_PER_HOUR + 7 * MS_PER_DAY)
    } else {
        None
    };
    let purged_at_ms = if phase >= SegmentPhase::Purged {
        Some(created_at_ms + 24 * MS_PER_HOUR + 7 * MS_PER_DAY + 30 * MS_PER_DAY)
    } else {
        None
    };
    SegmentMeta {
        segment_id: id.to_string(),
        sensitivity: tier,
        phase,
        start_ordinal: 0,
        end_ordinal: Some(100),
        size_bytes: 1024,
        created_at_ms,
        sealed_at_ms,
        archived_at_ms,
        purged_at_ms,
        event_count: 100,
    }
}

// =============================================================================
// 1. RetentionConfig defaults
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn config_default_values_correct(_dummy in 0u8..1) {
        let cfg = RetentionConfig::default();
        prop_assert_eq!(cfg.hot_hours, 24, "hot_hours default should be 24");
        prop_assert_eq!(cfg.warm_days, 7, "warm_days default should be 7");
        prop_assert_eq!(cfg.cold_days, 30, "cold_days default should be 30");
        prop_assert_eq!(cfg.t3_max_hours, 24, "t3_max_hours default should be 24");
        prop_assert_eq!(cfg.t1_extended_days, 30, "t1_extended_days default should be 30");
        prop_assert_eq!(cfg.max_segment_bytes, 256 * 1024 * 1024, "max_segment_bytes default should be 256MB");
        prop_assert_eq!(cfg.max_segment_duration_secs, 3600, "max_segment_duration_secs default should be 3600");
    }
}

// =============================================================================
// 2. RetentionConfig::default().validate() succeeds
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn config_default_validates_ok(_dummy in 0u8..1) {
        let result = RetentionConfig::default().validate();
        let is_ok = result.is_ok();
        prop_assert!(is_ok, "default config should validate successfully");
    }
}

// =============================================================================
// 3. RetentionConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_roundtrip(cfg in arb_valid_config()) {
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: RetentionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.hot_hours, cfg.hot_hours, "hot_hours mismatch after roundtrip");
        prop_assert_eq!(parsed.warm_days, cfg.warm_days, "warm_days mismatch after roundtrip");
        prop_assert_eq!(parsed.cold_days, cfg.cold_days, "cold_days mismatch after roundtrip");
        prop_assert_eq!(parsed.t3_max_hours, cfg.t3_max_hours, "t3_max_hours mismatch after roundtrip");
        prop_assert_eq!(parsed.t1_extended_days, cfg.t1_extended_days, "t1_extended_days mismatch after roundtrip");
        prop_assert_eq!(parsed.max_segment_bytes, cfg.max_segment_bytes, "max_segment_bytes mismatch after roundtrip");
        prop_assert_eq!(parsed.max_segment_duration_secs, cfg.max_segment_duration_secs, "max_segment_duration_secs mismatch after roundtrip");
    }
}

// =============================================================================
// 4. RetentionConfig::validate() rejects zero hot_hours
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_validate_rejects_zero_hot_hours(cfg in arb_valid_config()) {
        let mut bad = cfg;
        bad.hot_hours = 0;
        let result = bad.validate();
        let is_err = result.is_err();
        prop_assert!(is_err, "config with hot_hours=0 should fail validation");
    }
}

// =============================================================================
// 5. RetentionConfig::validate() rejects zero warm_days
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_validate_rejects_zero_warm_days(cfg in arb_valid_config()) {
        let mut bad = cfg;
        bad.warm_days = 0;
        let result = bad.validate();
        let is_err = result.is_err();
        prop_assert!(is_err, "config with warm_days=0 should fail validation");
    }
}

// =============================================================================
// 6. RetentionConfig::validate() rejects zero cold_days
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_validate_rejects_zero_cold_days(cfg in arb_valid_config()) {
        let mut bad = cfg;
        bad.cold_days = 0;
        let result = bad.validate();
        let is_err = result.is_err();
        prop_assert!(is_err, "config with cold_days=0 should fail validation");
    }
}

// =============================================================================
// 7. RetentionConfig::validate() rejects t1_extended_days > 90
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_validate_rejects_t1_extended_over_90(
        cfg in arb_valid_config(),
        over in 91u32..=500
    ) {
        let mut bad = cfg;
        bad.t1_extended_days = over;
        let result = bad.validate();
        let is_err = result.is_err();
        prop_assert!(is_err, "config with t1_extended_days={} should fail validation", over);
    }
}

// =============================================================================
// 8. RetentionConfig::validate() rejects zero max_segment_bytes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_validate_rejects_zero_max_segment_bytes(cfg in arb_valid_config()) {
        let mut bad = cfg;
        bad.max_segment_bytes = 0;
        let result = bad.validate();
        let is_err = result.is_err();
        prop_assert!(is_err, "config with max_segment_bytes=0 should fail validation");
    }
}

// =============================================================================
// 9. RetentionConfig::validate() rejects zero max_segment_duration_secs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_validate_rejects_zero_max_segment_duration_secs(cfg in arb_valid_config()) {
        let mut bad = cfg;
        bad.max_segment_duration_secs = 0;
        let result = bad.validate();
        let is_err = result.is_err();
        prop_assert!(is_err, "config with max_segment_duration_secs=0 should fail validation");
    }
}

// =============================================================================
// 10. Valid configs always pass validation
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn config_valid_always_passes(cfg in arb_valid_config()) {
        let result = cfg.validate();
        let is_ok = result.is_ok();
        prop_assert!(is_ok, "all valid configs should pass validation");
    }
}

// =============================================================================
// 11. RetentionConfig::retention_hours() correctness
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn config_retention_hours_per_tier(cfg in arb_valid_config()) {
        let t1_hours = cfg.retention_hours(SensitivityTier::T1Standard);
        let expected_t1 = (cfg.t1_extended_days as u64) * 24;
        prop_assert_eq!(t1_hours, expected_t1, "T1 retention_hours mismatch");

        let t2_hours = cfg.retention_hours(SensitivityTier::T2Sensitive);
        let expected_t2 = (cfg.hot_hours as u64)
            + (cfg.warm_days as u64) * 24
            + (cfg.cold_days as u64) * 24;
        prop_assert_eq!(t2_hours, expected_t2, "T2 retention_hours mismatch");

        let t3_hours = cfg.retention_hours(SensitivityTier::T3Restricted);
        let expected_t3 = cfg.t3_max_hours as u64;
        prop_assert_eq!(t3_hours, expected_t3, "T3 retention_hours mismatch");
    }
}

// =============================================================================
// 12. SensitivityTier serde roundtrip (snake_case)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn tier_serde_roundtrip(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let parsed: SensitivityTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, tier, "SensitivityTier serde roundtrip failed");
        // Verify snake_case format
        let json_str = json.as_str();
        let valid_snake = matches!(
            json_str,
            "\"t1_standard\"" | "\"t2_sensitive\"" | "\"t3_restricted\""
        );
        prop_assert!(valid_snake, "expected snake_case serde, got {}", json);
    }
}

// =============================================================================
// 13. SensitivityTier ordering (T1 < T2 < T3)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn tier_ordering_correct(_dummy in 0u8..1) {
        let t1_lt_t2 = SensitivityTier::T1Standard < SensitivityTier::T2Sensitive;
        prop_assert!(t1_lt_t2, "T1 should be < T2");
        let t2_lt_t3 = SensitivityTier::T2Sensitive < SensitivityTier::T3Restricted;
        prop_assert!(t2_lt_t3, "T2 should be < T3");
        let t1_lt_t3 = SensitivityTier::T1Standard < SensitivityTier::T3Restricted;
        prop_assert!(t1_lt_t3, "T1 should be < T3");
    }
}

// =============================================================================
// 14. SensitivityTier::classify() — all combinations
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn tier_classify_unredacted_always_t3(
        redaction in arb_redaction()
    ) {
        let tier = SensitivityTier::classify(redaction, true);
        prop_assert_eq!(tier, SensitivityTier::T3Restricted,
            "unredacted_capture=true should always yield T3");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn tier_classify_no_redaction_no_capture_is_t1(_dummy in 0u8..1) {
        let tier = SensitivityTier::classify(RecorderRedactionLevel::None, false);
        prop_assert_eq!(tier, SensitivityTier::T1Standard,
            "None redaction + no capture should be T1");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn tier_classify_partial_redaction_no_capture_is_t2(_dummy in 0u8..1) {
        let tier = SensitivityTier::classify(RecorderRedactionLevel::Partial, false);
        prop_assert_eq!(tier, SensitivityTier::T2Sensitive,
            "Partial redaction + no capture should be T2");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn tier_classify_full_redaction_no_capture_is_t2(_dummy in 0u8..1) {
        let tier = SensitivityTier::classify(RecorderRedactionLevel::Full, false);
        prop_assert_eq!(tier, SensitivityTier::T2Sensitive,
            "Full redaction + no capture should be T2");
    }
}

// =============================================================================
// 15. SensitivityTier::requires_accelerated_purge() only T3
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn tier_accelerated_purge_only_t3(tier in arb_tier()) {
        let needs_accel = tier.requires_accelerated_purge();
        let is_t3 = tier == SensitivityTier::T3Restricted;
        prop_assert_eq!(needs_accel, is_t3,
            "requires_accelerated_purge should only be true for T3, got tier={:?}", tier);
    }
}

// =============================================================================
// 16. SegmentPhase serde roundtrip (snake_case)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn phase_serde_roundtrip(phase in arb_phase()) {
        let json = serde_json::to_string(&phase).unwrap();
        let parsed: SegmentPhase = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, phase, "SegmentPhase serde roundtrip failed");
        let json_str = json.as_str();
        let valid_snake = matches!(
            json_str,
            "\"active\"" | "\"sealed\"" | "\"archived\"" | "\"purged\""
        );
        prop_assert!(valid_snake, "expected snake_case serde, got {}", json);
    }
}

// =============================================================================
// 17. SegmentPhase ordering (Active < Sealed < Archived < Purged)
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn phase_ordering_correct(_dummy in 0u8..1) {
        let a_lt_s = SegmentPhase::Active < SegmentPhase::Sealed;
        prop_assert!(a_lt_s, "Active should be < Sealed");
        let s_lt_ar = SegmentPhase::Sealed < SegmentPhase::Archived;
        prop_assert!(s_lt_ar, "Sealed should be < Archived");
        let ar_lt_p = SegmentPhase::Archived < SegmentPhase::Purged;
        prop_assert!(ar_lt_p, "Archived should be < Purged");
    }
}

// =============================================================================
// 18. SegmentPhase::is_writable() only Active
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn phase_writable_only_active(phase in arb_phase()) {
        let writable = phase.is_writable();
        let is_active = matches!(phase, SegmentPhase::Active);
        prop_assert_eq!(writable, is_active,
            "is_writable should only be true for Active, got phase={:?}", phase);
    }
}

// =============================================================================
// 19. SegmentPhase::is_queryable() — Active/Sealed/Archived but not Purged
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn phase_queryable_not_purged(phase in arb_phase()) {
        let queryable = phase.is_queryable();
        let is_purged = matches!(phase, SegmentPhase::Purged);
        prop_assert_eq!(queryable, !is_purged,
            "is_queryable should be true for all phases except Purged, got phase={:?}", phase);
    }
}

// =============================================================================
// 20. SegmentPhase::can_transition_to() validates linear progression
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn phase_valid_forward_transitions(_dummy in 0u8..1) {
        let a_to_s = SegmentPhase::Active.can_transition_to(SegmentPhase::Sealed);
        prop_assert!(a_to_s, "Active -> Sealed should be valid");
        let s_to_ar = SegmentPhase::Sealed.can_transition_to(SegmentPhase::Archived);
        prop_assert!(s_to_ar, "Sealed -> Archived should be valid");
        let ar_to_p = SegmentPhase::Archived.can_transition_to(SegmentPhase::Purged);
        prop_assert!(ar_to_p, "Archived -> Purged should be valid");
    }
}

// =============================================================================
// 21. SegmentPhase::can_transition_to() rejects backward/skip transitions
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn phase_rejects_backward_transitions(
        from in arb_phase(),
        to in arb_phase()
    ) {
        let can = from.can_transition_to(to);
        // Valid transitions: Active->Sealed, Sealed->Archived, Archived->Purged
        let should_be_valid = matches!(
            (from, to),
            (SegmentPhase::Active, SegmentPhase::Sealed)
            | (SegmentPhase::Sealed, SegmentPhase::Archived)
            | (SegmentPhase::Archived, SegmentPhase::Purged)
        );
        prop_assert_eq!(can, should_be_valid,
            "transition {:?} -> {:?}: expected {}, got {}", from, to, should_be_valid, can);
    }
}

// =============================================================================
// 22. SegmentMeta::make_id() format
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn segment_make_id_format(
        ordinal in 0u64..100_000,
        tier in arb_tier(),
        created_ms in 0u64..10_000_000_000
    ) {
        let id = SegmentMeta::make_id(ordinal, tier, created_ms);
        let tier_label = match tier {
            SensitivityTier::T1Standard => "t1",
            SensitivityTier::T2Sensitive => "t2",
            SensitivityTier::T3Restricted => "t3",
        };
        let expected = format!("{}_{}_{}",ordinal, tier_label, created_ms);
        prop_assert_eq!(id, expected, "make_id format mismatch");
    }
}

// =============================================================================
// 23. SegmentMeta::should_roll() by size
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn segment_should_roll_by_size(
        max_bytes in 1u64..1_000_000,
        size_bytes in 0u64..2_000_000,
        created_at_ms in 0u64..1_000_000
    ) {
        let cfg = RetentionConfig {
            max_segment_bytes: max_bytes,
            max_segment_duration_secs: u64::MAX / 1000, // effectively disable time-based roll
            ..Default::default()
        };
        let seg = make_active_segment("test", SensitivityTier::T1Standard, created_at_ms, size_bytes, 10);
        // now_ms = created_at_ms so age is 0 (no time-based roll)
        let rolled = seg.should_roll(&cfg, created_at_ms);
        let expected = size_bytes >= max_bytes;
        prop_assert_eq!(rolled, expected,
            "should_roll size: size_bytes={}, max_bytes={}", size_bytes, max_bytes);
    }
}

// =============================================================================
// 24. SegmentMeta::should_roll() by time
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn segment_should_roll_by_time(
        max_duration_secs in 1u64..86_400,
        age_secs in 0u64..172_800,
        created_at_ms in 0u64..1_000_000_000
    ) {
        let cfg = RetentionConfig {
            max_segment_bytes: u64::MAX, // effectively disable size-based roll
            max_segment_duration_secs: max_duration_secs,
            ..Default::default()
        };
        let seg = make_active_segment("test", SensitivityTier::T1Standard, created_at_ms, 0, 10);
        let now_ms = created_at_ms + age_secs * 1000;
        let rolled = seg.should_roll(&cfg, now_ms);
        let expected = age_secs >= max_duration_secs;
        prop_assert_eq!(rolled, expected,
            "should_roll time: age_secs={}, max_duration_secs={}", age_secs, max_duration_secs);
    }
}

// =============================================================================
// 25. SegmentMeta::should_roll() false for non-Active phases
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn segment_no_roll_unless_active(
        phase in prop_oneof![
            Just(SegmentPhase::Sealed),
            Just(SegmentPhase::Archived),
            Just(SegmentPhase::Purged),
        ]
    ) {
        let cfg = RetentionConfig {
            max_segment_bytes: 1, // would roll if active
            max_segment_duration_secs: 1, // would roll if active
            ..Default::default()
        };
        let seg = make_segment_at_phase("test", SensitivityTier::T1Standard, phase, 0);
        let rolled = seg.should_roll(&cfg, 999_999_999);
        prop_assert!(!rolled, "non-Active phase {:?} should never roll", phase);
    }
}

// =============================================================================
// 26. SegmentMeta::transition() updates timestamps correctly
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn segment_transition_sets_sealed_timestamp(
        now_ms in 1u64..10_000_000_000
    ) {
        let mut seg = make_active_segment("s1", SensitivityTier::T1Standard, 0, 100, 10);
        let result = seg.transition(SegmentPhase::Sealed, now_ms);
        let is_ok = result.is_ok();
        prop_assert!(is_ok, "Active -> Sealed transition should succeed");
        prop_assert_eq!(seg.phase, SegmentPhase::Sealed, "phase should be Sealed");
        prop_assert_eq!(seg.sealed_at_ms, Some(now_ms), "sealed_at_ms should be set");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn segment_transition_sets_archived_timestamp(
        now_ms in 1u64..10_000_000_000
    ) {
        let mut seg = make_active_segment("s1", SensitivityTier::T1Standard, 0, 100, 10);
        seg.transition(SegmentPhase::Sealed, 1000).unwrap();
        let result = seg.transition(SegmentPhase::Archived, now_ms);
        let is_ok = result.is_ok();
        prop_assert!(is_ok, "Sealed -> Archived transition should succeed");
        prop_assert_eq!(seg.phase, SegmentPhase::Archived, "phase should be Archived");
        prop_assert_eq!(seg.archived_at_ms, Some(now_ms), "archived_at_ms should be set");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn segment_transition_sets_purged_timestamp(
        now_ms in 1u64..10_000_000_000
    ) {
        let mut seg = make_active_segment("s1", SensitivityTier::T1Standard, 0, 100, 10);
        seg.transition(SegmentPhase::Sealed, 1000).unwrap();
        seg.transition(SegmentPhase::Archived, 2000).unwrap();
        let result = seg.transition(SegmentPhase::Purged, now_ms);
        let is_ok = result.is_ok();
        prop_assert!(is_ok, "Archived -> Purged transition should succeed");
        prop_assert_eq!(seg.phase, SegmentPhase::Purged, "phase should be Purged");
        prop_assert_eq!(seg.purged_at_ms, Some(now_ms), "purged_at_ms should be set");
    }
}

// =============================================================================
// 27. SegmentMeta::transition() rejects invalid transitions
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn segment_transition_rejects_invalid(
        from in arb_phase(),
        to in arb_phase()
    ) {
        let should_be_valid = matches!(
            (from, to),
            (SegmentPhase::Active, SegmentPhase::Sealed)
            | (SegmentPhase::Sealed, SegmentPhase::Archived)
            | (SegmentPhase::Archived, SegmentPhase::Purged)
        );

        let mut seg = make_segment_at_phase("s1", SensitivityTier::T1Standard, from, 0);
        let result = seg.transition(to, 999_999);
        if should_be_valid {
            let is_ok = result.is_ok();
            prop_assert!(is_ok, "expected valid transition {:?} -> {:?} to succeed", from, to);
        } else {
            let is_err = result.is_err();
            prop_assert!(is_err, "expected invalid transition {:?} -> {:?} to fail", from, to);
        }
    }
}

// =============================================================================
// 28. RetentionManager basic operations
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn manager_add_and_count(
        n in 0usize..20
    ) {
        let mut mgr = RetentionManager::with_defaults();
        for i in 0..n {
            let id = format!("seg_{}", i);
            mgr.add_segment(make_active_segment(&id, SensitivityTier::T1Standard, 0, 100, 10));
        }
        prop_assert_eq!(mgr.segment_count(), n, "segment_count should match add count");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn manager_get_segment_by_id(
        n in 1usize..10,
        query_idx in 0usize..10
    ) {
        let mut mgr = RetentionManager::with_defaults();
        for i in 0..n {
            let id = format!("seg_{}", i);
            mgr.add_segment(make_active_segment(&id, SensitivityTier::T1Standard, 0, 100, 10));
        }
        let query_id = format!("seg_{}", query_idx);
        let found = mgr.get_segment(&query_id).is_some();
        let expected = query_idx < n;
        prop_assert_eq!(found, expected,
            "get_segment('{}') expected found={}, n={}", query_id, expected, n);
    }
}

// =============================================================================
// 29. RetentionManager segments_in_phase filtering
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn manager_segments_in_phase_filter(
        n_active in 0usize..5,
        n_sealed in 0usize..5,
        n_archived in 0usize..5
    ) {
        let mut mgr = RetentionManager::with_defaults();
        let mut idx = 0usize;
        for _ in 0..n_active {
            let id = format!("seg_{}", idx);
            mgr.add_segment(make_segment_at_phase(&id, SensitivityTier::T1Standard, SegmentPhase::Active, 0));
            idx += 1;
        }
        for _ in 0..n_sealed {
            let id = format!("seg_{}", idx);
            mgr.add_segment(make_segment_at_phase(&id, SensitivityTier::T1Standard, SegmentPhase::Sealed, 0));
            idx += 1;
        }
        for _ in 0..n_archived {
            let id = format!("seg_{}", idx);
            mgr.add_segment(make_segment_at_phase(&id, SensitivityTier::T1Standard, SegmentPhase::Archived, 0));
            idx += 1;
        }
        prop_assert_eq!(mgr.segments_in_phase(SegmentPhase::Active).len(), n_active,
            "active count mismatch");
        prop_assert_eq!(mgr.segments_in_phase(SegmentPhase::Sealed).len(), n_sealed,
            "sealed count mismatch");
        prop_assert_eq!(mgr.segments_in_phase(SegmentPhase::Archived).len(), n_archived,
            "archived count mismatch");
    }
}

// =============================================================================
// 30. RetentionManager segments_by_tier filtering
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn manager_segments_by_tier_filter(
        n_t1 in 0usize..5,
        n_t2 in 0usize..5,
        n_t3 in 0usize..5
    ) {
        let mut mgr = RetentionManager::with_defaults();
        let mut idx = 0usize;
        for _ in 0..n_t1 {
            let id = format!("seg_{}", idx);
            mgr.add_segment(make_active_segment(&id, SensitivityTier::T1Standard, 0, 100, 10));
            idx += 1;
        }
        for _ in 0..n_t2 {
            let id = format!("seg_{}", idx);
            mgr.add_segment(make_active_segment(&id, SensitivityTier::T2Sensitive, 0, 100, 10));
            idx += 1;
        }
        for _ in 0..n_t3 {
            let id = format!("seg_{}", idx);
            mgr.add_segment(make_active_segment(&id, SensitivityTier::T3Restricted, 0, 100, 10));
            idx += 1;
        }
        prop_assert_eq!(mgr.segments_by_tier(SensitivityTier::T1Standard).len(), n_t1,
            "T1 count mismatch");
        prop_assert_eq!(mgr.segments_by_tier(SensitivityTier::T2Sensitive).len(), n_t2,
            "T2 count mismatch");
        prop_assert_eq!(mgr.segments_by_tier(SensitivityTier::T3Restricted).len(), n_t3,
            "T3 count mismatch");
    }
}

// =============================================================================
// 31. RetentionSweepResult default is empty
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn sweep_result_default_empty(_dummy in 0u8..1) {
        let result = RetentionSweepResult::default();
        prop_assert!(result.sealed.is_empty(), "sealed should be empty");
        prop_assert!(result.archived.is_empty(), "archived should be empty");
        prop_assert!(result.purge_candidates.is_empty(), "purge_candidates should be empty");
        prop_assert!(result.purged.is_empty(), "purged should be empty");
        prop_assert!(result.held.is_empty(), "held should be empty");
    }
}

// =============================================================================
// 32. RetentionStats::live_count() and live_bytes() sums
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn stats_live_count_and_bytes(
        active_count in 0usize..100,
        active_bytes in 0u64..1_000_000,
        sealed_count in 0usize..100,
        sealed_bytes in 0u64..1_000_000,
        archived_count in 0usize..100,
        archived_bytes in 0u64..1_000_000,
        purged_count in 0usize..100
    ) {
        let stats = RetentionStats {
            active_count,
            active_bytes,
            sealed_count,
            sealed_bytes,
            archived_count,
            archived_bytes,
            purged_count,
        };
        let expected_count = active_count + sealed_count + archived_count;
        let expected_bytes = active_bytes + sealed_bytes + archived_bytes;
        prop_assert_eq!(stats.live_count(), expected_count, "live_count mismatch");
        prop_assert_eq!(stats.live_bytes(), expected_bytes, "live_bytes mismatch");
    }
}

// =============================================================================
// 33. RetentionStats from manager matches manual computation
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn manager_stats_match_segment_data(
        n_active in 0usize..4,
        n_sealed in 0usize..4,
        active_size in 100u64..10_000,
        sealed_size in 100u64..10_000
    ) {
        let mut mgr = RetentionManager::with_defaults();
        let mut idx = 0usize;
        for _ in 0..n_active {
            let id = format!("seg_{}", idx);
            mgr.add_segment(make_active_segment(&id, SensitivityTier::T1Standard, 0, active_size, 10));
            idx += 1;
        }
        for _ in 0..n_sealed {
            let id = format!("seg_{}", idx);
            let mut seg = make_segment_at_phase(&id, SensitivityTier::T2Sensitive, SegmentPhase::Sealed, 0);
            seg.size_bytes = sealed_size;
            mgr.add_segment(seg);
            idx += 1;
        }
        let stats = mgr.stats();
        prop_assert_eq!(stats.active_count, n_active, "active_count mismatch");
        prop_assert_eq!(stats.sealed_count, n_sealed, "sealed_count mismatch");
        prop_assert_eq!(stats.active_bytes, (n_active as u64) * active_size, "active_bytes mismatch");
        prop_assert_eq!(stats.sealed_bytes, (n_sealed as u64) * sealed_size, "sealed_bytes mismatch");
    }
}

// =============================================================================
// 34. RetentionAuditType serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn audit_type_serde_roundtrip(audit_type in arb_audit_type()) {
        let json = serde_json::to_string(&audit_type).unwrap();
        let parsed: RetentionAuditType = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, audit_type, "RetentionAuditType serde roundtrip failed");
    }
}

// =============================================================================
// 35. RetentionAuditEvent serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn audit_event_serde_roundtrip(
        audit_type in arb_audit_type(),
        tier in arb_tier(),
        from_phase in prop::option::of(arb_phase()),
        to_phase in arb_phase(),
        timestamp_ms in 0u64..10_000_000_000u64
    ) {
        let event = RetentionAuditEvent {
            audit_version: "ft.recorder.audit.v1".to_string(),
            event_type: audit_type,
            segment_id: "0_t2_1000".to_string(),
            ordinal_range: Some((0, 100)),
            sensitivity: tier,
            from_phase,
            to_phase,
            timestamp_ms,
            justification: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let parsed: RetentionAuditEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.audit_version, event.audit_version, "audit_version mismatch");
        prop_assert_eq!(parsed.event_type, event.event_type, "event_type mismatch");
        prop_assert_eq!(parsed.segment_id, event.segment_id, "segment_id mismatch");
        prop_assert_eq!(parsed.sensitivity, event.sensitivity, "sensitivity mismatch");
        prop_assert_eq!(parsed.from_phase, event.from_phase, "from_phase mismatch");
        prop_assert_eq!(parsed.to_phase, event.to_phase, "to_phase mismatch");
        prop_assert_eq!(parsed.timestamp_ms, event.timestamp_ms, "timestamp_ms mismatch");
    }
}

// =============================================================================
// 36. RetentionError Display messages
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn error_display_invalid_config(msg in "[a-z ]{1,50}") {
        let err = RetentionError::InvalidConfig(msg.clone());
        let display = err.to_string();
        let contains_msg = display.contains(&msg);
        prop_assert!(contains_msg, "Display should contain message: {}", display);
        let contains_prefix = display.contains("invalid retention config");
        prop_assert!(contains_prefix, "Display should contain prefix: {}", display);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn error_display_invalid_transition(
        seg_id in "[a-z0-9_]{1,20}",
        from in arb_phase(),
        to in arb_phase()
    ) {
        let err = RetentionError::InvalidTransition {
            segment_id: seg_id.clone(),
            from,
            to,
        };
        let display = err.to_string();
        let contains_id = display.contains(&seg_id);
        prop_assert!(contains_id, "Display should contain segment_id: {}", display);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn error_display_checkpoint_hold(
        seg_id in "[a-z0-9_]{1,20}",
        consumer in "[a-z]{1,20}"
    ) {
        let err = RetentionError::CheckpointHold {
            segment_id: seg_id.clone(),
            consumer: consumer.clone(),
        };
        let display = err.to_string();
        let contains_consumer = display.contains(&consumer);
        prop_assert!(contains_consumer, "Display should contain consumer: {}", display);
        let contains_id = display.contains(&seg_id);
        prop_assert!(contains_id, "Display should contain segment_id: {}", display);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn error_display_not_found(id in "[a-z0-9_]{1,20}") {
        let err = RetentionError::NotFound(id.clone());
        let display = err.to_string();
        let contains_id = display.contains(&id);
        prop_assert!(contains_id, "Display should contain id: {}", display);
        let contains_prefix = display.contains("not found");
        prop_assert!(contains_prefix, "Display should contain 'not found': {}", display);
    }
}

// =============================================================================
// 37. SegmentMeta eligible_transition: Active -> Sealed at hot_hours threshold
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn eligible_transition_active_to_sealed_threshold(
        hot_hours in 1u32..=168,
        age_hours in 0u64..=336
    ) {
        let cfg = RetentionConfig {
            hot_hours,
            ..Default::default()
        };
        // Non-T3 so accelerated purge does not interfere
        let seg = make_active_segment("s1", SensitivityTier::T2Sensitive, 0, 100, 10);
        let now_ms = age_hours * MS_PER_HOUR;
        let result = seg.eligible_transition(&cfg, now_ms);
        if age_hours >= hot_hours as u64 {
            prop_assert_eq!(result, Some(SegmentPhase::Sealed),
                "should transition at age_hours={}, hot_hours={}", age_hours, hot_hours);
        } else {
            prop_assert_eq!(result, None,
                "should not transition at age_hours={}, hot_hours={}", age_hours, hot_hours);
        }
    }
}

// =============================================================================
// 38. SegmentMeta eligible_transition: Sealed -> Archived at warm_days
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn eligible_transition_sealed_to_archived_threshold(
        warm_days in 1u32..=30,
        sealed_age_days in 0u64..=60
    ) {
        let cfg = RetentionConfig {
            warm_days,
            // Set hot_hours far away so T3 accelerated purge doesn't trigger
            hot_hours: 24,
            ..Default::default()
        };
        let created_at_ms = 0u64;
        let sealed_at_ms = 24 * MS_PER_HOUR;
        let mut seg = make_active_segment("s1", SensitivityTier::T2Sensitive, created_at_ms, 100, 10);
        seg.transition(SegmentPhase::Sealed, sealed_at_ms).unwrap();

        let now_ms = sealed_at_ms + sealed_age_days * MS_PER_DAY;
        let result = seg.eligible_transition(&cfg, now_ms);
        if sealed_age_days >= warm_days as u64 {
            prop_assert_eq!(result, Some(SegmentPhase::Archived),
                "sealed {} days old should transition with warm_days={}", sealed_age_days, warm_days);
        } else {
            prop_assert_eq!(result, None,
                "sealed {} days old should not transition with warm_days={}", sealed_age_days, warm_days);
        }
    }
}

// =============================================================================
// 39. SegmentMeta eligible_transition: Archived -> Purged with T1 extended
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn eligible_transition_t1_extended_vs_cold(
        cold_days in 1u32..=60,
        t1_extended_days in 1u32..=90,
        archived_age_days in 0u64..=120
    ) {
        let cfg = RetentionConfig {
            cold_days,
            t1_extended_days,
            ..Default::default()
        };
        let created_at_ms = 0u64;
        let sealed_at_ms = 24 * MS_PER_HOUR;
        let archived_at_ms = sealed_at_ms + 7 * MS_PER_DAY;

        // T1 segment
        let mut seg_t1 = make_active_segment("t1_seg", SensitivityTier::T1Standard, created_at_ms, 100, 10);
        seg_t1.transition(SegmentPhase::Sealed, sealed_at_ms).unwrap();
        seg_t1.transition(SegmentPhase::Archived, archived_at_ms).unwrap();

        let now_ms = archived_at_ms + archived_age_days * MS_PER_DAY;
        let t1_result = seg_t1.eligible_transition(&cfg, now_ms);
        if archived_age_days >= t1_extended_days as u64 {
            prop_assert_eq!(t1_result, Some(SegmentPhase::Purged),
                "T1 should purge at {} days (t1_extended_days={})", archived_age_days, t1_extended_days);
        } else {
            prop_assert_eq!(t1_result, None,
                "T1 should not purge at {} days (t1_extended_days={})", archived_age_days, t1_extended_days);
        }

        // T2 segment (uses cold_days instead)
        let mut seg_t2 = make_active_segment("t2_seg", SensitivityTier::T2Sensitive, created_at_ms, 100, 10);
        seg_t2.transition(SegmentPhase::Sealed, sealed_at_ms).unwrap();
        seg_t2.transition(SegmentPhase::Archived, archived_at_ms).unwrap();

        let t2_result = seg_t2.eligible_transition(&cfg, now_ms);
        if archived_age_days >= cold_days as u64 {
            prop_assert_eq!(t2_result, Some(SegmentPhase::Purged),
                "T2 should purge at {} days (cold_days={})", archived_age_days, cold_days);
        } else {
            prop_assert_eq!(t2_result, None,
                "T2 should not purge at {} days (cold_days={})", archived_age_days, cold_days);
        }
    }
}

// =============================================================================
// 40. T3 accelerated purge behavior
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn t3_accelerated_purge_triggers_at_threshold(
        t3_max_hours in 1u32..=48,
        age_hours in 0u64..=96
    ) {
        let cfg = RetentionConfig {
            t3_max_hours,
            hot_hours: 240, // very large so normal threshold doesn't trigger
            ..Default::default()
        };
        let seg = make_active_segment("t3_seg", SensitivityTier::T3Restricted, 0, 100, 10);
        let now_ms = age_hours * MS_PER_HOUR;
        let result = seg.eligible_transition(&cfg, now_ms);

        if age_hours >= t3_max_hours as u64 {
            // T3 accelerated purge triggers — Active transitions to Sealed
            prop_assert_eq!(result, Some(SegmentPhase::Sealed),
                "T3 Active should transition to Sealed at age_hours={}, t3_max_hours={}", age_hours, t3_max_hours);
        } else {
            // Below T3 max hours AND below hot_hours (240) — no transition
            prop_assert_eq!(result, None,
                "T3 Active should not transition at age_hours={}, t3_max_hours={}", age_hours, t3_max_hours);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn t3_accelerated_sealed_to_archived(
        t3_max_hours in 1u32..=24
    ) {
        let cfg = RetentionConfig {
            t3_max_hours,
            warm_days: 365, // very large so normal threshold doesn't trigger
            ..Default::default()
        };
        let created_at_ms = 0u64;
        let sealed_at_ms = 1 * MS_PER_HOUR; // sealed very quickly

        let mut seg = make_active_segment("t3_seg", SensitivityTier::T3Restricted, created_at_ms, 100, 10);
        seg.transition(SegmentPhase::Sealed, sealed_at_ms).unwrap();

        // Advance past t3_max_hours
        let now_ms = (t3_max_hours as u64 + 1) * MS_PER_HOUR;
        let result = seg.eligible_transition(&cfg, now_ms);
        prop_assert_eq!(result, Some(SegmentPhase::Archived),
            "T3 Sealed should accelerate to Archived past t3_max_hours");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn t3_accelerated_archived_to_purged(
        t3_max_hours in 1u32..=24
    ) {
        let cfg = RetentionConfig {
            t3_max_hours,
            cold_days: 365, // very large so normal threshold doesn't trigger
            ..Default::default()
        };
        let created_at_ms = 0u64;
        let sealed_at_ms = 1 * MS_PER_HOUR;
        let archived_at_ms = 2 * MS_PER_HOUR;

        let mut seg = make_active_segment("t3_seg", SensitivityTier::T3Restricted, created_at_ms, 100, 10);
        seg.transition(SegmentPhase::Sealed, sealed_at_ms).unwrap();
        seg.transition(SegmentPhase::Archived, archived_at_ms).unwrap();

        // Advance past t3_max_hours
        let now_ms = (t3_max_hours as u64 + 1) * MS_PER_HOUR;
        let result = seg.eligible_transition(&cfg, now_ms);
        prop_assert_eq!(result, Some(SegmentPhase::Purged),
            "T3 Archived should accelerate to Purged past t3_max_hours");
    }
}

// =============================================================================
// 41. RetentionManager sweep seals then archives then purges
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn manager_sweep_full_lifecycle(
        hot_hours in 1u32..=48,
        warm_days in 1u32..=14,
        cold_days in 1u32..=30
    ) {
        let cfg = RetentionConfig {
            hot_hours,
            warm_days,
            cold_days,
            ..Default::default()
        };
        let mut mgr = RetentionManager::new(cfg).unwrap();
        mgr.add_segment(make_active_segment("s1", SensitivityTier::T2Sensitive, 0, 100, 10));

        let empty_holders = HashMap::new();

        // Sweep at hot_hours => seal
        let now_seal = (hot_hours as u64) * MS_PER_HOUR;
        let r1 = mgr.sweep(now_seal, &empty_holders);
        prop_assert_eq!(r1.sealed.len(), 1, "should seal 1 segment");

        // Sweep at warm_days after sealing => archive
        let now_archive = now_seal + (warm_days as u64) * MS_PER_DAY;
        let r2 = mgr.sweep(now_archive, &empty_holders);
        prop_assert_eq!(r2.archived.len(), 1, "should archive 1 segment");

        // Sweep at cold_days after archiving => purge
        let now_purge = now_archive + (cold_days as u64) * MS_PER_DAY;
        let r3 = mgr.sweep(now_purge, &empty_holders);
        prop_assert_eq!(r3.purged.len(), 1, "should purge 1 segment");
    }
}

// =============================================================================
// 42. RetentionManager sweep blocks purge on checkpoint holder
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn manager_sweep_blocks_purge_with_holders(
        consumer in "[a-z]{3,10}"
    ) {
        let cfg = RetentionConfig {
            hot_hours: 1,
            warm_days: 1,
            cold_days: 1,
            ..Default::default()
        };
        let mut mgr = RetentionManager::new(cfg).unwrap();
        let mut seg = make_active_segment("s1", SensitivityTier::T2Sensitive, 0, 100, 10);
        seg.transition(SegmentPhase::Sealed, MS_PER_HOUR).unwrap();
        seg.transition(SegmentPhase::Archived, MS_PER_HOUR + MS_PER_DAY).unwrap();
        mgr.add_segment(seg);

        let mut holders = HashMap::new();
        holders.insert("s1".to_string(), vec![consumer.clone()]);

        let now = MS_PER_HOUR + MS_PER_DAY + 2 * MS_PER_DAY;
        let result = mgr.sweep(now, &holders);
        prop_assert!(result.purged.is_empty(), "purge should be blocked by holder");
        let has_held = !result.held.is_empty();
        prop_assert!(has_held, "held list should not be empty");
    }
}

// =============================================================================
// 43. RetentionManager total_data_bytes excludes purged
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn manager_total_bytes_excludes_purged(
        active_size in 0u64..1_000_000,
        sealed_size in 0u64..1_000_000,
        purged_size in 0u64..1_000_000
    ) {
        let mut mgr = RetentionManager::with_defaults();

        let mut s_active = make_active_segment("a1", SensitivityTier::T1Standard, 0, active_size, 10);
        mgr.add_segment(s_active);

        let mut s_sealed = make_segment_at_phase("s1", SensitivityTier::T1Standard, SegmentPhase::Sealed, 0);
        s_sealed.size_bytes = sealed_size;
        mgr.add_segment(s_sealed);

        let mut s_purged = make_segment_at_phase("p1", SensitivityTier::T1Standard, SegmentPhase::Purged, 0);
        s_purged.size_bytes = purged_size;
        mgr.add_segment(s_purged);

        let total = mgr.total_data_bytes();
        let expected = active_size + sealed_size;
        prop_assert_eq!(total, expected,
            "total_data_bytes should exclude purged segments");
    }
}

// =============================================================================
// 44. RetentionManager total_events excludes purged
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn manager_total_events_excludes_purged(
        active_events in 0u64..10_000,
        purged_events in 0u64..10_000
    ) {
        let mut mgr = RetentionManager::with_defaults();
        mgr.add_segment(make_active_segment("a1", SensitivityTier::T1Standard, 0, 100, active_events));

        let mut s_purged = make_segment_at_phase("p1", SensitivityTier::T1Standard, SegmentPhase::Purged, 0);
        s_purged.event_count = purged_events;
        mgr.add_segment(s_purged);

        let total = mgr.total_events();
        prop_assert_eq!(total, active_events,
            "total_events should exclude purged segments");
    }
}

// =============================================================================
// 45. Purged segments are not queryable
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1))]

    #[test]
    fn purged_phase_not_queryable(_dummy in 0u8..1) {
        let purged = SegmentPhase::Purged;
        let queryable = purged.is_queryable();
        prop_assert!(!queryable, "Purged phase should not be queryable");
        let writable = purged.is_writable();
        prop_assert!(!writable, "Purged phase should not be writable");
        let transitions = purged.valid_transitions();
        prop_assert!(transitions.is_empty(), "Purged should have no valid transitions");
    }
}
