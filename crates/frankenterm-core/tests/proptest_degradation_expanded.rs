//! Expanded property-based tests for degradation.rs.
//!
//! Focuses on `evaluate_resize_degradation_ladder` classifier invariants:
//! tier escalation ordering, signal→tier mapping correctness, assessment field
//! consistency, warning_line behavior, and quality/correctness/availability
//! reductions monotonicity.
//!
//! Complements proptest_degradation.rs which covers DegradationMonitor state
//! transitions and Subsystem/OverallStatus serde.

use frankenterm_core::degradation::{
    ResizeDegradationAssessment, ResizeDegradationSignals, ResizeDegradationTier,
    evaluate_resize_degradation_ladder,
};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_tier() -> impl Strategy<Value = ResizeDegradationTier> {
    prop_oneof![
        Just(ResizeDegradationTier::FullQuality),
        Just(ResizeDegradationTier::QualityReduced),
        Just(ResizeDegradationTier::CorrectnessGuarded),
        Just(ResizeDegradationTier::EmergencyCompatibility),
    ]
}

/// Arbitrary signals with reasonable ranges.
fn arb_signals() -> impl Strategy<Value = ResizeDegradationSignals> {
    (
        0_usize..20,   // stalled_total
        0_usize..10,   // stalled_critical
        1000_u64..30_000, // warning_threshold_ms
        5000_u64..60_000, // critical_threshold_ms
        1_usize..10,   // critical_stalled_limit
        any::<bool>(), // safe_mode_recommended
        any::<bool>(), // safe_mode_active
        any::<bool>(), // legacy_fallback_enabled
    )
        .prop_map(
            |(
                stalled_total,
                stalled_critical,
                warning_threshold_ms,
                critical_threshold_ms,
                critical_stalled_limit,
                safe_mode_recommended,
                safe_mode_active,
                legacy_fallback_enabled,
            )| {
                ResizeDegradationSignals {
                    stalled_total,
                    stalled_critical,
                    warning_threshold_ms,
                    critical_threshold_ms,
                    critical_stalled_limit,
                    safe_mode_recommended,
                    safe_mode_active,
                    legacy_fallback_enabled,
                }
            },
        )
}

/// Signals known to produce FullQuality tier.
fn signals_full_quality() -> impl Strategy<Value = ResizeDegradationSignals> {
    (
        1000_u64..30_000,
        5000_u64..60_000,
        1_usize..10,
        any::<bool>(),
    )
        .prop_map(
            |(warning_ms, critical_ms, limit, legacy)| ResizeDegradationSignals {
                stalled_total: 0,
                stalled_critical: 0,
                warning_threshold_ms: warning_ms,
                critical_threshold_ms: critical_ms,
                critical_stalled_limit: limit,
                safe_mode_recommended: false,
                safe_mode_active: false,
                legacy_fallback_enabled: legacy,
            },
        )
}

/// Signals known to produce QualityReduced tier.
fn signals_quality_reduced() -> impl Strategy<Value = ResizeDegradationSignals> {
    (
        1_usize..20,    // stalled_total > 0
        1000_u64..30_000,
        5000_u64..60_000,
        1_usize..10,
        any::<bool>(),
    )
        .prop_map(
            |(stalled, warning_ms, critical_ms, limit, legacy)| ResizeDegradationSignals {
                stalled_total: stalled,
                stalled_critical: 0,
                warning_threshold_ms: warning_ms,
                critical_threshold_ms: critical_ms,
                critical_stalled_limit: limit,
                safe_mode_recommended: false,
                safe_mode_active: false,
                legacy_fallback_enabled: legacy,
            },
        )
}

/// Signals known to produce CorrectnessGuarded tier.
fn signals_correctness_guarded() -> impl Strategy<Value = ResizeDegradationSignals> {
    (
        0_usize..20,    // stalled_total
        1_usize..10,    // stalled_critical > 0
        1000_u64..30_000,
        5000_u64..60_000,
        1_usize..10,
        any::<bool>(),
    )
        .prop_map(
            |(stalled_total, stalled_critical, warning_ms, critical_ms, limit, legacy)| {
                ResizeDegradationSignals {
                    stalled_total,
                    stalled_critical,
                    warning_threshold_ms: warning_ms,
                    critical_threshold_ms: critical_ms,
                    critical_stalled_limit: limit,
                    safe_mode_recommended: false,
                    safe_mode_active: false,
                    legacy_fallback_enabled: legacy,
                }
            },
        )
}

/// Signals known to produce EmergencyCompatibility tier.
fn signals_emergency() -> impl Strategy<Value = ResizeDegradationSignals> {
    (
        0_usize..20,
        0_usize..10,
        1000_u64..30_000,
        5000_u64..60_000,
        1_usize..10,
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(
            |(stalled_total, stalled_critical, warning_ms, critical_ms, limit, recommended, legacy)| {
                ResizeDegradationSignals {
                    stalled_total,
                    stalled_critical,
                    warning_threshold_ms: warning_ms,
                    critical_threshold_ms: critical_ms,
                    critical_stalled_limit: limit,
                    safe_mode_recommended: recommended,
                    safe_mode_active: true, // Key: safe_mode_active = true
                    legacy_fallback_enabled: legacy,
                }
            },
        )
}

// =========================================================================
// Tier classification correctness
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Zero stalls + no safe mode → FullQuality.
    #[test]
    fn full_quality_when_no_stalls(signals in signals_full_quality()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(assessment.tier, ResizeDegradationTier::FullQuality,
            "zero stalls and no safe mode should yield FullQuality");
    }

    /// stalled_total > 0 but stalled_critical == 0 and no safe mode → QualityReduced.
    #[test]
    fn quality_reduced_when_warning_stalls(signals in signals_quality_reduced()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(assessment.tier, ResizeDegradationTier::QualityReduced,
            "warning stalls with no critical stalls should yield QualityReduced");
    }

    /// stalled_critical > 0 (without safe_mode_active) → CorrectnessGuarded.
    #[test]
    fn correctness_guarded_when_critical_stalls(signals in signals_correctness_guarded()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(assessment.tier, ResizeDegradationTier::CorrectnessGuarded,
            "critical stalls should yield CorrectnessGuarded");
    }

    /// safe_mode_active = true → EmergencyCompatibility regardless of other signals.
    #[test]
    fn emergency_when_safe_mode_active(signals in signals_emergency()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(assessment.tier, ResizeDegradationTier::EmergencyCompatibility,
            "safe_mode_active should always yield EmergencyCompatibility");
    }

    /// safe_mode_recommended = true (without safe_mode_active) → CorrectnessGuarded.
    #[test]
    fn correctness_guarded_when_safe_mode_recommended(
        stalled_total in 0_usize..20,
        warning_ms in 1000_u64..30_000,
        critical_ms in 5000_u64..60_000,
        limit in 1_usize..10,
        legacy in any::<bool>(),
    ) {
        let signals = ResizeDegradationSignals {
            stalled_total,
            stalled_critical: 0,
            warning_threshold_ms: warning_ms,
            critical_threshold_ms: critical_ms,
            critical_stalled_limit: limit,
            safe_mode_recommended: true,
            safe_mode_active: false,
            legacy_fallback_enabled: legacy,
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(assessment.tier, ResizeDegradationTier::CorrectnessGuarded,
            "safe_mode_recommended should yield CorrectnessGuarded");
    }
}

// =========================================================================
// Tier ordering and rank invariants
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// ResizeDegradationTier ordering: FullQuality < QualityReduced < CorrectnessGuarded < Emergency.
    #[test]
    fn tier_ordering_strict(_dummy in 0..1_u8) {
        prop_assert!(ResizeDegradationTier::FullQuality < ResizeDegradationTier::QualityReduced);
        prop_assert!(ResizeDegradationTier::QualityReduced < ResizeDegradationTier::CorrectnessGuarded);
        prop_assert!(ResizeDegradationTier::CorrectnessGuarded < ResizeDegradationTier::EmergencyCompatibility);
    }

    /// rank() is monotonically increasing with tier ordering.
    #[test]
    fn rank_monotonic_with_ordering(a in arb_tier(), b in arb_tier()) {
        if a < b {
            prop_assert!(a.rank() < b.rank(),
                "if a < b then rank(a) < rank(b)");
        } else if a == b {
            prop_assert_eq!(a.rank(), b.rank(),
                "equal tiers must have equal rank");
        } else {
            prop_assert!(a.rank() > b.rank(),
                "if a > b then rank(a) > rank(b)");
        }
    }

    /// rank() values span 0..=3 exactly.
    #[test]
    fn rank_range(tier in arb_tier()) {
        prop_assert!(tier.rank() <= 3, "rank should be 0-3");
    }

    /// FullQuality rank is 0.
    #[test]
    fn full_quality_rank_zero(_dummy in 0..1_u8) {
        prop_assert_eq!(ResizeDegradationTier::FullQuality.rank(), 0);
    }

    /// EmergencyCompatibility rank is 3.
    #[test]
    fn emergency_rank_three(_dummy in 0..1_u8) {
        prop_assert_eq!(ResizeDegradationTier::EmergencyCompatibility.rank(), 3);
    }

    /// tier_rank in assessment always matches tier.rank().
    #[test]
    fn assessment_tier_rank_consistent(signals in arb_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(assessment.tier_rank, assessment.tier.rank(),
            "assessment tier_rank must match tier.rank()");
    }
}

// =========================================================================
// Assessment field consistency
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Assessment always has non-empty trigger_condition.
    #[test]
    fn assessment_trigger_nonempty(signals in arb_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert!(!assessment.trigger_condition.is_empty(),
            "trigger_condition must not be empty");
    }

    /// Assessment always has non-empty recovery_rule.
    #[test]
    fn assessment_recovery_nonempty(signals in arb_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert!(!assessment.recovery_rule.is_empty(),
            "recovery_rule must not be empty");
    }

    /// Assessment always has non-empty recommended_action.
    #[test]
    fn assessment_action_nonempty(signals in arb_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert!(!assessment.recommended_action.is_empty(),
            "recommended_action must not be empty");
    }

    /// Assessment signals field matches input signals.
    #[test]
    fn assessment_signals_preserved(signals in arb_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals.clone());
        prop_assert_eq!(assessment.signals, signals,
            "assessment should preserve input signals");
    }

    /// Assessment serde roundtrip preserves all fields.
    #[test]
    fn assessment_serde_roundtrip(signals in arb_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        let json = serde_json::to_string(&assessment).unwrap();
        let back: ResizeDegradationAssessment = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.tier, assessment.tier);
        prop_assert_eq!(back.tier_rank, assessment.tier_rank);
        prop_assert_eq!(&back.trigger_condition, &assessment.trigger_condition);
        prop_assert_eq!(&back.recovery_rule, &assessment.recovery_rule);
        prop_assert_eq!(&back.recommended_action, &assessment.recommended_action);
        prop_assert_eq!(back.quality_reductions.len(), assessment.quality_reductions.len());
        prop_assert_eq!(back.correctness_guards.len(), assessment.correctness_guards.len());
        prop_assert_eq!(back.availability_changes.len(), assessment.availability_changes.len());
    }
}

// =========================================================================
// Quality reductions / correctness guards / availability changes monotonicity
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// FullQuality has no quality reductions.
    #[test]
    fn full_quality_no_reductions(signals in signals_full_quality()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert!(assessment.quality_reductions.is_empty(),
            "FullQuality should have no quality reductions");
        prop_assert!(assessment.correctness_guards.is_empty(),
            "FullQuality should have no correctness guards");
        prop_assert!(assessment.availability_changes.is_empty(),
            "FullQuality should have no availability changes");
    }

    /// QualityReduced has quality reductions but no correctness guards.
    #[test]
    fn quality_reduced_has_reductions_only(signals in signals_quality_reduced()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert!(!assessment.quality_reductions.is_empty(),
            "QualityReduced should have quality reductions");
        prop_assert!(assessment.correctness_guards.is_empty(),
            "QualityReduced should not have correctness guards");
        prop_assert!(assessment.availability_changes.is_empty(),
            "QualityReduced should not have availability changes");
    }

    /// CorrectnessGuarded has quality reductions AND correctness guards.
    #[test]
    fn correctness_guarded_has_reductions_and_guards(signals in signals_correctness_guarded()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert!(!assessment.quality_reductions.is_empty(),
            "CorrectnessGuarded should have quality reductions");
        prop_assert!(!assessment.correctness_guards.is_empty(),
            "CorrectnessGuarded should have correctness guards");
        prop_assert!(assessment.availability_changes.is_empty(),
            "CorrectnessGuarded should not have availability changes");
    }

    /// EmergencyCompatibility has ALL three categories populated.
    #[test]
    fn emergency_has_all_categories(signals in signals_emergency()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert!(!assessment.quality_reductions.is_empty(),
            "EmergencyCompatibility should have quality reductions");
        prop_assert!(!assessment.correctness_guards.is_empty(),
            "EmergencyCompatibility should have correctness guards");
        prop_assert!(!assessment.availability_changes.is_empty(),
            "EmergencyCompatibility should have availability changes");
    }

    /// Reduction categories are cumulative: higher tier >= lower tier category counts.
    #[test]
    fn categories_cumulative(signals in arb_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        let has_quality = !assessment.quality_reductions.is_empty();
        let has_correctness = !assessment.correctness_guards.is_empty();
        let has_availability = !assessment.availability_changes.is_empty();

        // availability implies correctness implies quality
        if has_availability {
            prop_assert!(has_correctness,
                "availability changes should imply correctness guards");
        }
        if has_correctness {
            prop_assert!(has_quality,
                "correctness guards should imply quality reductions");
        }
    }
}

// =========================================================================
// warning_line behavior
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// FullQuality warning_line is None.
    #[test]
    fn full_quality_no_warning_line(signals in signals_full_quality()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert!(assessment.warning_line().is_none(),
            "FullQuality should not produce a warning line");
    }

    /// Non-FullQuality tiers always produce a warning_line.
    #[test]
    fn degraded_tiers_have_warning_line(signals in arb_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        if assessment.tier != ResizeDegradationTier::FullQuality {
            prop_assert!(assessment.warning_line().is_some(),
                "degraded tier {:?} should produce a warning line", assessment.tier);
        }
    }

    /// Warning line contains the tier name for degraded tiers.
    #[test]
    fn warning_line_mentions_tier(signals in arb_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        if let Some(line) = assessment.warning_line() {
            let tier_name = match assessment.tier {
                ResizeDegradationTier::QualityReduced => "quality-reduced",
                ResizeDegradationTier::CorrectnessGuarded => "correctness-guarded",
                ResizeDegradationTier::EmergencyCompatibility => "emergency compatibility",
                ResizeDegradationTier::FullQuality => unreachable!(),
            };
            prop_assert!(line.contains(tier_name),
                "warning line should mention tier name '{}' but got: {}", tier_name, line);
        }
    }

    /// EmergencyCompatibility warning line mentions legacy fallback when enabled.
    #[test]
    fn emergency_warning_mentions_legacy(
        stalled_total in 0_usize..10,
        stalled_critical in 0_usize..10,
    ) {
        let signals = ResizeDegradationSignals {
            stalled_total,
            stalled_critical,
            warning_threshold_ms: 3000,
            critical_threshold_ms: 10000,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: true,
            legacy_fallback_enabled: true,
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        let line = assessment.warning_line().expect("emergency should have warning line");
        prop_assert!(line.contains("legacy fallback"),
            "emergency with legacy_fallback_enabled should mention it: {}", line);
    }

    /// EmergencyCompatibility warning line without legacy fallback doesn't mention it.
    #[test]
    fn emergency_warning_no_legacy_when_disabled(
        stalled_total in 0_usize..10,
        stalled_critical in 0_usize..10,
    ) {
        let signals = ResizeDegradationSignals {
            stalled_total,
            stalled_critical,
            warning_threshold_ms: 3000,
            critical_threshold_ms: 10000,
            critical_stalled_limit: 3,
            safe_mode_recommended: false,
            safe_mode_active: true,
            legacy_fallback_enabled: false,
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        let line = assessment.warning_line().expect("emergency should have warning line");
        prop_assert!(!line.contains("legacy fallback"),
            "emergency without legacy should not mention it: {}", line);
    }
}

// =========================================================================
// Tier serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// ResizeDegradationTier serde roundtrip.
    #[test]
    fn tier_serde_roundtrip(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let back: ResizeDegradationTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, tier);
    }

    /// ResizeDegradationTier serde uses snake_case.
    #[test]
    fn tier_serde_snake_case(tier in arb_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let expected = match tier {
            ResizeDegradationTier::FullQuality => "\"full_quality\"",
            ResizeDegradationTier::QualityReduced => "\"quality_reduced\"",
            ResizeDegradationTier::CorrectnessGuarded => "\"correctness_guarded\"",
            ResizeDegradationTier::EmergencyCompatibility => "\"emergency_compatibility\"",
        };
        prop_assert_eq!(&json, expected);
    }

    /// ResizeDegradationTier Display matches serde.
    #[test]
    fn tier_display_matches_serde(tier in arb_tier()) {
        let display = tier.to_string();
        let serde_str = serde_json::to_string(&tier).unwrap();
        // serde has quotes, Display does not
        prop_assert_eq!(format!("\"{}\"", display), serde_str);
    }

    /// ResizeDegradationSignals serde roundtrip.
    #[test]
    fn signals_serde_roundtrip(signals in arb_signals()) {
        let json = serde_json::to_string(&signals).unwrap();
        let back: ResizeDegradationSignals = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, signals);
    }
}

// =========================================================================
// Determinism
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// evaluate_resize_degradation_ladder is deterministic.
    #[test]
    fn ladder_deterministic(signals in arb_signals()) {
        let a = evaluate_resize_degradation_ladder(signals.clone());
        let b = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(a.tier, b.tier);
        prop_assert_eq!(a.tier_rank, b.tier_rank);
        prop_assert_eq!(&a.trigger_condition, &b.trigger_condition);
        prop_assert_eq!(&a.recovery_rule, &b.recovery_rule);
    }

    /// Tier ordering is transitive.
    #[test]
    fn tier_ordering_transitive(a in arb_tier(), b in arb_tier(), c in arb_tier()) {
        if a < b && b < c {
            prop_assert!(a < c, "tier ordering should be transitive");
        }
    }

    /// Tier ordering is antisymmetric.
    #[test]
    fn tier_ordering_antisymmetric(a in arb_tier(), b in arb_tier()) {
        if a < b {
            prop_assert!(!(b < a), "tier ordering should be antisymmetric");
        }
    }

    /// Tier ordering is total.
    #[test]
    fn tier_ordering_total(a in arb_tier(), b in arb_tier()) {
        prop_assert!(a < b || a == b || a > b, "tier ordering should be total");
    }
}

// =========================================================================
// Edge cases as plain tests
// =========================================================================

#[test]
fn all_zero_signals_is_full_quality() {
    let signals = ResizeDegradationSignals {
        stalled_total: 0,
        stalled_critical: 0,
        warning_threshold_ms: 3000,
        critical_threshold_ms: 10000,
        critical_stalled_limit: 3,
        safe_mode_recommended: false,
        safe_mode_active: false,
        legacy_fallback_enabled: false,
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    assert_eq!(assessment.tier, ResizeDegradationTier::FullQuality);
    assert_eq!(assessment.tier_rank, 0);
    assert!(assessment.warning_line().is_none());
}

#[test]
fn safe_mode_active_trumps_everything() {
    let signals = ResizeDegradationSignals {
        stalled_total: 0,
        stalled_critical: 0,
        warning_threshold_ms: 3000,
        critical_threshold_ms: 10000,
        critical_stalled_limit: 3,
        safe_mode_recommended: false,
        safe_mode_active: true,
        legacy_fallback_enabled: false,
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    assert_eq!(assessment.tier, ResizeDegradationTier::EmergencyCompatibility);
}

#[test]
fn safe_mode_recommended_alone_triggers_correctness_guarded() {
    let signals = ResizeDegradationSignals {
        stalled_total: 0,
        stalled_critical: 0,
        warning_threshold_ms: 3000,
        critical_threshold_ms: 10000,
        critical_stalled_limit: 3,
        safe_mode_recommended: true,
        safe_mode_active: false,
        legacy_fallback_enabled: false,
    };
    let assessment = evaluate_resize_degradation_ladder(signals);
    assert_eq!(assessment.tier, ResizeDegradationTier::CorrectnessGuarded);
}

#[test]
fn tier_rank_values_are_0_1_2_3() {
    assert_eq!(ResizeDegradationTier::FullQuality.rank(), 0);
    assert_eq!(ResizeDegradationTier::QualityReduced.rank(), 1);
    assert_eq!(ResizeDegradationTier::CorrectnessGuarded.rank(), 2);
    assert_eq!(ResizeDegradationTier::EmergencyCompatibility.rank(), 3);
}
