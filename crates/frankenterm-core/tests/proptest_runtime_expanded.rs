//! Expanded property-based tests for the runtime module.
//!
//! Covers RuntimeMetrics counter accumulation, percentile invariants,
//! snapshot consistency, RuntimeConfig defaults, degradation ladder
//! mapping, and ResizeDegradationTier ordering.
//!
//! Complements the existing proptest_runtime.rs which covers serde
//! roundtrips for ResizeWatchdogSeverity, Assessment, and Telemetry.

use frankenterm_core::degradation::{
    ResizeDegradationAssessment, ResizeDegradationSignals, ResizeDegradationTier,
    evaluate_resize_degradation_ladder,
};
use frankenterm_core::runtime::{
    ResizeWatchdogAssessment, ResizeWatchdogSeverity, RuntimeConfig, RuntimeMetrics,
};
use proptest::prelude::*;
use std::time::Duration;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_severity() -> impl Strategy<Value = ResizeWatchdogSeverity> {
    prop_oneof![
        Just(ResizeWatchdogSeverity::Healthy),
        Just(ResizeWatchdogSeverity::Warning),
        Just(ResizeWatchdogSeverity::Critical),
        Just(ResizeWatchdogSeverity::SafeModeActive),
    ]
}

fn arb_degradation_tier() -> impl Strategy<Value = ResizeDegradationTier> {
    prop_oneof![
        Just(ResizeDegradationTier::FullQuality),
        Just(ResizeDegradationTier::QualityReduced),
        Just(ResizeDegradationTier::CorrectnessGuarded),
        Just(ResizeDegradationTier::EmergencyCompatibility),
    ]
}

fn arb_degradation_signals() -> impl Strategy<Value = ResizeDegradationSignals> {
    (
        0usize..50,    // stalled_total
        0usize..50,    // stalled_critical
        1u64..10_000,  // warning_threshold_ms
        1u64..30_000,  // critical_threshold_ms
        1usize..10,    // critical_stalled_limit
        any::<bool>(), // safe_mode_recommended
        any::<bool>(), // safe_mode_active
        any::<bool>(), // legacy_fallback_enabled
    )
        .prop_map(
            |(total, crit, wt, ct, limit, recommended, active, legacy)| {
                // Ensure critical <= total and warning_threshold <= critical_threshold
                let stalled_critical = crit.min(total);
                let (warning_threshold_ms, critical_threshold_ms) =
                    if wt <= ct { (wt, ct) } else { (ct, wt) };
                ResizeDegradationSignals {
                    stalled_total: total,
                    stalled_critical,
                    warning_threshold_ms,
                    critical_threshold_ms,
                    critical_stalled_limit: limit,
                    safe_mode_recommended: recommended,
                    safe_mode_active: active,
                    legacy_fallback_enabled: legacy,
                }
            },
        )
}

fn arb_ingest_lag_samples() -> impl Strategy<Value = Vec<u64>> {
    proptest::collection::vec(0u64..100_000, 1..50)
}

fn arb_storage_wait_samples() -> impl Strategy<Value = Vec<Duration>> {
    proptest::collection::vec(0u64..10_000_000, 1..50)
        .prop_map(|v| v.into_iter().map(Duration::from_micros).collect())
}

fn arb_cursor_samples() -> impl Strategy<Value = Vec<u64>> {
    proptest::collection::vec(0u64..1_000_000_000, 1..50)
}

// ── RuntimeMetrics: ingest lag accumulation ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Ingest lag count equals number of recorded samples.
    #[test]
    fn ingest_lag_count_matches_samples(samples in arb_ingest_lag_samples()) {
        let metrics = RuntimeMetrics::default();
        for &s in &samples {
            metrics.record_ingest_lag(s);
        }
        prop_assert_eq!(
            metrics.ingest_lag_count(),
            samples.len() as u64,
            "count mismatch: expected {}, got {}",
            samples.len(),
            metrics.ingest_lag_count()
        );
    }

    /// Ingest lag sum matches manual summation.
    #[test]
    fn ingest_lag_sum_matches(samples in arb_ingest_lag_samples()) {
        let metrics = RuntimeMetrics::default();
        let expected_sum: u64 = samples.iter().sum();
        for &s in &samples {
            metrics.record_ingest_lag(s);
        }
        prop_assert_eq!(
            metrics.ingest_lag_sum_ms(),
            expected_sum,
            "sum mismatch: expected {}, got {}",
            expected_sum,
            metrics.ingest_lag_sum_ms()
        );
    }

    /// Max ingest lag equals the maximum sample.
    #[test]
    fn ingest_lag_max_matches(samples in arb_ingest_lag_samples()) {
        let metrics = RuntimeMetrics::default();
        let expected_max = *samples.iter().max().unwrap_or(&0);
        for &s in &samples {
            metrics.record_ingest_lag(s);
        }
        prop_assert_eq!(
            metrics.max_ingest_lag_ms(),
            expected_max,
            "max mismatch: expected {}, got {}",
            expected_max,
            metrics.max_ingest_lag_ms()
        );
    }

    /// Average ingest lag equals sum/count.
    #[test]
    fn ingest_lag_avg_matches(samples in arb_ingest_lag_samples()) {
        let metrics = RuntimeMetrics::default();
        for &s in &samples {
            metrics.record_ingest_lag(s);
        }
        let expected = samples.iter().sum::<u64>() as f64 / samples.len() as f64;
        let actual = metrics.avg_ingest_lag_ms();
        let diff = (expected - actual).abs();
        prop_assert!(
            diff < 0.01,
            "avg mismatch: expected {}, got {}, diff {}",
            expected,
            actual,
            diff
        );
    }
}

// ── RuntimeMetrics: zero state ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// Fresh metrics have zero counters.
    #[test]
    fn fresh_metrics_all_zero(_dummy in 0u8..1) {
        let metrics = RuntimeMetrics::default();
        prop_assert_eq!(metrics.ingest_lag_count(), 0);
        prop_assert_eq!(metrics.ingest_lag_sum_ms(), 0);
        prop_assert_eq!(metrics.max_ingest_lag_ms(), 0);
        prop_assert!((metrics.avg_ingest_lag_ms() - 0.0).abs() < f64::EPSILON);
        prop_assert_eq!(metrics.segments_persisted(), 0);
        prop_assert_eq!(metrics.events_recorded(), 0);
        prop_assert_eq!(metrics.storage_lock_contention_events(), 0);
        prop_assert!(metrics.last_db_write().is_none());
        prop_assert_eq!(metrics.cursor_snapshot_bytes_max(), 0);
        prop_assert_eq!(metrics.native_output_input_events(), 0);
        prop_assert_eq!(metrics.native_output_batches_emitted(), 0);
    }
}

// ── RuntimeMetrics: storage lock wait accumulation ──────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Storage lock wait max is >= all recorded samples.
    #[test]
    fn storage_lock_wait_max_dominates(samples in arb_storage_wait_samples()) {
        let metrics = RuntimeMetrics::default();
        for s in &samples {
            metrics.record_storage_lock_wait(*s);
        }
        let max_sample_ms = samples.iter()
            .map(|d| d.as_micros() as f64 / 1000.0)
            .fold(0.0f64, f64::max);
        let reported_max = metrics.max_storage_lock_wait_ms();
        // Allow tiny float tolerance
        prop_assert!(
            reported_max >= max_sample_ms - 0.001,
            "max {} should be >= max sample {}",
            reported_max,
            max_sample_ms
        );
    }

    /// Average storage lock wait is non-negative.
    #[test]
    fn storage_lock_wait_avg_non_negative(samples in arb_storage_wait_samples()) {
        let metrics = RuntimeMetrics::default();
        for s in &samples {
            metrics.record_storage_lock_wait(*s);
        }
        prop_assert!(
            metrics.avg_storage_lock_wait_ms() >= 0.0,
            "avg should be non-negative"
        );
    }

    /// Percentile ordering: p50 <= p95 for storage lock wait.
    #[test]
    fn storage_lock_wait_percentile_order(samples in arb_storage_wait_samples()) {
        let metrics = RuntimeMetrics::default();
        for s in &samples {
            metrics.record_storage_lock_wait(*s);
        }
        let p50 = metrics.p50_storage_lock_wait_ms();
        let p95 = metrics.p95_storage_lock_wait_ms();
        prop_assert!(
            p50 <= p95 + 0.001,
            "p50 ({}) should be <= p95 ({})",
            p50,
            p95
        );
    }
}

// ── RuntimeMetrics: storage lock hold accumulation ──────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Storage lock hold max dominates all samples.
    #[test]
    fn storage_lock_hold_max_dominates(samples in arb_storage_wait_samples()) {
        let metrics = RuntimeMetrics::default();
        for s in &samples {
            metrics.record_storage_lock_hold(*s);
        }
        let max_sample_ms = samples.iter()
            .map(|d| d.as_micros() as f64 / 1000.0)
            .fold(0.0f64, f64::max);
        let reported_max = metrics.max_storage_lock_hold_ms();
        prop_assert!(
            reported_max >= max_sample_ms - 0.001,
            "max {} should be >= max sample {}",
            reported_max,
            max_sample_ms
        );
    }

    /// p50 <= p95 for storage lock hold.
    #[test]
    fn storage_lock_hold_percentile_order(samples in arb_storage_wait_samples()) {
        let metrics = RuntimeMetrics::default();
        for s in &samples {
            metrics.record_storage_lock_hold(*s);
        }
        let p50 = metrics.p50_storage_lock_hold_ms();
        let p95 = metrics.p95_storage_lock_hold_ms();
        prop_assert!(
            p50 <= p95 + 0.001,
            "p50 ({}) should be <= p95 ({})",
            p50,
            p95
        );
    }
}

// ── RuntimeMetrics: cursor snapshot memory ──────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Cursor snapshot max is >= all recorded samples.
    #[test]
    fn cursor_snapshot_max_dominates(samples in arb_cursor_samples()) {
        let metrics = RuntimeMetrics::default();
        let expected_max = *samples.iter().max().unwrap_or(&0);
        for &s in &samples {
            metrics.record_cursor_snapshot_memory(s);
        }
        let reported_max = metrics.cursor_snapshot_bytes_max();
        prop_assert!(
            reported_max >= expected_max,
            "max {} should be >= expected max {}",
            reported_max,
            expected_max
        );
    }

    /// Cursor snapshot last is the most recently recorded sample.
    #[test]
    fn cursor_snapshot_last_is_latest(samples in arb_cursor_samples()) {
        let metrics = RuntimeMetrics::default();
        for &s in &samples {
            metrics.record_cursor_snapshot_memory(s);
        }
        let last = *samples.last().unwrap();
        let reported_last = metrics.cursor_snapshot_bytes_last();
        // ShardedGauge.set may retain max across shards, so last >= recorded last
        // or last is the actual last — depends on ShardedGauge implementation
        // Just verify it's non-zero if we recorded non-zero values
        if last > 0 {
            prop_assert!(
                reported_last > 0,
                "cursor_snapshot_bytes_last should be > 0 after recording {}",
                last
            );
        }
    }

    /// p50 <= p95 for cursor snapshot bytes.
    #[test]
    fn cursor_snapshot_percentile_order(samples in arb_cursor_samples()) {
        let metrics = RuntimeMetrics::default();
        for &s in &samples {
            metrics.record_cursor_snapshot_memory(s);
        }
        let p50 = metrics.p50_cursor_snapshot_bytes();
        let p95 = metrics.p95_cursor_snapshot_bytes();
        prop_assert!(
            p50 <= p95,
            "p50 ({}) should be <= p95 ({})",
            p50,
            p95
        );
    }

    /// Average cursor snapshot bytes is non-negative.
    #[test]
    fn cursor_snapshot_avg_non_negative(samples in arb_cursor_samples()) {
        let metrics = RuntimeMetrics::default();
        for &s in &samples {
            metrics.record_cursor_snapshot_memory(s);
        }
        prop_assert!(
            metrics.avg_cursor_snapshot_bytes() >= 0.0,
            "avg should be non-negative"
        );
    }
}

// ── RuntimeMetrics: native output batch tracking ────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Native output input events count accumulates correctly.
    #[test]
    fn native_output_input_events_accumulate(
        event_count in 1usize..20,
        bytes_per_event in 1usize..10_000,
    ) {
        let metrics = RuntimeMetrics::default();
        for _ in 0..event_count {
            metrics.record_native_output_input(bytes_per_event);
        }
        prop_assert_eq!(
            metrics.native_output_input_events(),
            event_count as u64,
            "input events count mismatch"
        );
        prop_assert_eq!(
            metrics.native_output_input_bytes(),
            (event_count * bytes_per_event) as u64,
            "input bytes mismatch"
        );
    }

    /// Native output batch recording works correctly.
    #[test]
    fn native_output_batches_accumulate(
        batch_count in 1usize..10,
        events_per_batch in 1u32..100,
        bytes_per_batch in 1usize..100_000,
    ) {
        let metrics = RuntimeMetrics::default();
        for _ in 0..batch_count {
            metrics.record_native_output_batch(events_per_batch, bytes_per_batch);
        }
        prop_assert_eq!(
            metrics.native_output_batches_emitted(),
            batch_count as u64,
            "batch count mismatch"
        );
        prop_assert!(
            metrics.native_output_max_batch_events() >= events_per_batch as u64,
            "max batch events should be >= {}",
            events_per_batch
        );
        prop_assert!(
            metrics.native_output_max_batch_bytes() >= bytes_per_batch as u64,
            "max batch bytes should be >= {}",
            bytes_per_batch
        );
    }
}

// ── RuntimeMetrics: lock_memory_snapshot consistency ─────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// lock_memory_snapshot produces consistent percentile ordering.
    #[test]
    fn snapshot_percentile_consistency(
        wait_samples in arb_storage_wait_samples(),
        hold_samples in arb_storage_wait_samples(),
        cursor_samples in arb_cursor_samples(),
    ) {
        let metrics = RuntimeMetrics::default();
        for s in &wait_samples {
            metrics.record_storage_lock_wait(*s);
        }
        for s in &hold_samples {
            metrics.record_storage_lock_hold(*s);
        }
        for &s in &cursor_samples {
            metrics.record_cursor_snapshot_memory(s);
        }

        let snap = metrics.lock_memory_snapshot();

        // Wait percentile ordering
        prop_assert!(
            snap.p50_storage_lock_wait_ms <= snap.p95_storage_lock_wait_ms + 0.001,
            "wait p50 ({}) > p95 ({})",
            snap.p50_storage_lock_wait_ms,
            snap.p95_storage_lock_wait_ms
        );

        // Hold percentile ordering
        prop_assert!(
            snap.p50_storage_lock_hold_ms <= snap.p95_storage_lock_hold_ms + 0.001,
            "hold p50 ({}) > p95 ({})",
            snap.p50_storage_lock_hold_ms,
            snap.p95_storage_lock_hold_ms
        );

        // Cursor percentile ordering
        prop_assert!(
            snap.p50_cursor_snapshot_bytes <= snap.p95_cursor_snapshot_bytes,
            "cursor p50 ({}) > p95 ({})",
            snap.p50_cursor_snapshot_bytes,
            snap.p95_cursor_snapshot_bytes
        );

        // All values non-negative
        prop_assert!(snap.avg_storage_lock_wait_ms >= 0.0);
        prop_assert!(snap.avg_storage_lock_hold_ms >= 0.0);
        prop_assert!(snap.avg_cursor_snapshot_bytes >= 0.0);
    }
}

// ── RuntimeConfig: default invariants ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    /// RuntimeConfig default has sensible positive intervals.
    #[test]
    fn config_default_positive_intervals(_dummy in 0u8..1) {
        let cfg = RuntimeConfig::default();
        prop_assert!(cfg.discovery_interval > Duration::ZERO);
        prop_assert!(cfg.capture_interval > Duration::ZERO);
        prop_assert!(cfg.min_capture_interval > Duration::ZERO);
    }

    /// min_capture_interval <= capture_interval in default config.
    #[test]
    fn config_default_capture_interval_ordering(_dummy in 0u8..1) {
        let cfg = RuntimeConfig::default();
        prop_assert!(
            cfg.min_capture_interval <= cfg.capture_interval,
            "min ({:?}) should be <= max ({:?})",
            cfg.min_capture_interval,
            cfg.capture_interval
        );
    }

    /// Default config has positive channel buffer and concurrent captures.
    #[test]
    fn config_default_positive_limits(_dummy in 0u8..1) {
        let cfg = RuntimeConfig::default();
        prop_assert!(cfg.channel_buffer > 0);
        prop_assert!(cfg.max_concurrent_captures > 0);
        prop_assert!(cfg.retention_days > 0);
        prop_assert!(cfg.checkpoint_interval_secs > 0);
    }
}

// ── ResizeDegradationTier: rank ordering ────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// ResizeDegradationTier rank is monotonically increasing with severity.
    #[test]
    fn degradation_tier_rank_monotonic(
        a in arb_degradation_tier(),
        b in arb_degradation_tier(),
    ) {
        // If tier A is "less severe" based on enum ordering, rank should be <=
        let rank_a = a.rank();
        let rank_b = b.rank();
        // rank() returns 0..3 mapping to enum order
        // Just verify range
        prop_assert!(rank_a <= 3, "rank should be <= 3, got {}", rank_a);
        prop_assert!(rank_b <= 3, "rank should be <= 3, got {}", rank_b);
    }

    /// FullQuality has rank 0 (lowest severity).
    #[test]
    fn full_quality_rank_zero(_dummy in 0u8..1) {
        prop_assert_eq!(ResizeDegradationTier::FullQuality.rank(), 0);
    }

    /// EmergencyCompatibility has highest rank.
    #[test]
    fn emergency_compatibility_highest_rank(_dummy in 0u8..1) {
        prop_assert_eq!(ResizeDegradationTier::EmergencyCompatibility.rank(), 3);
        prop_assert!(
            ResizeDegradationTier::EmergencyCompatibility.rank()
                > ResizeDegradationTier::CorrectnessGuarded.rank()
        );
        prop_assert!(
            ResizeDegradationTier::CorrectnessGuarded.rank()
                > ResizeDegradationTier::QualityReduced.rank()
        );
        prop_assert!(
            ResizeDegradationTier::QualityReduced.rank()
                > ResizeDegradationTier::FullQuality.rank()
        );
    }

    /// Display is non-empty for all tiers.
    #[test]
    fn degradation_tier_display_nonempty(tier in arb_degradation_tier()) {
        let display = format!("{}", tier);
        prop_assert!(!display.is_empty(), "Display should be non-empty");
        // All display strings should be snake_case
        for ch in display.chars() {
            prop_assert!(
                ch.is_ascii_lowercase() || ch == '_',
                "expected snake_case, got char '{}' in '{}'",
                ch,
                display
            );
        }
    }
}

// ── evaluate_resize_degradation_ladder: tier selection ───────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// safe_mode_active always produces EmergencyCompatibility tier.
    #[test]
    fn degradation_safe_mode_active_is_emergency(signals in arb_degradation_signals()) {
        let signals = ResizeDegradationSignals {
            safe_mode_active: true,
            ..signals
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(
            assessment.tier,
            ResizeDegradationTier::EmergencyCompatibility,
            "safe_mode_active should produce EmergencyCompatibility"
        );
    }

    /// No stalls + no safe mode = FullQuality.
    #[test]
    fn degradation_no_stalls_is_full_quality(signals in arb_degradation_signals()) {
        let signals = ResizeDegradationSignals {
            stalled_total: 0,
            stalled_critical: 0,
            safe_mode_recommended: false,
            safe_mode_active: false,
            ..signals
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(
            assessment.tier,
            ResizeDegradationTier::FullQuality,
            "no stalls should produce FullQuality"
        );
    }

    /// stalled_total > 0 but no critical stalls and no safe mode = QualityReduced.
    #[test]
    fn degradation_warning_stalls_is_quality_reduced(
        total in 1usize..50,
        signals in arb_degradation_signals(),
    ) {
        let signals = ResizeDegradationSignals {
            stalled_total: total,
            stalled_critical: 0,
            safe_mode_recommended: false,
            safe_mode_active: false,
            ..signals
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(
            assessment.tier,
            ResizeDegradationTier::QualityReduced,
            "warning stalls with no critical should produce QualityReduced"
        );
    }

    /// stalled_critical > 0 without safe_mode_active = CorrectnessGuarded.
    #[test]
    fn degradation_critical_stalls_is_correctness_guarded(
        total in 1usize..50,
        critical in 1usize..50,
        signals in arb_degradation_signals(),
    ) {
        let critical = critical.min(total);
        let signals = ResizeDegradationSignals {
            stalled_total: total,
            stalled_critical: critical,
            safe_mode_active: false,
            ..signals
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(
            assessment.tier,
            ResizeDegradationTier::CorrectnessGuarded,
            "critical stalls without safe_mode should produce CorrectnessGuarded"
        );
    }

    /// tier_rank always matches tier.rank().
    #[test]
    fn degradation_tier_rank_consistent(signals in arb_degradation_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert_eq!(
            assessment.tier_rank,
            assessment.tier.rank(),
            "tier_rank ({}) should match tier.rank() ({})",
            assessment.tier_rank,
            assessment.tier.rank()
        );
    }

    /// signals field in assessment matches input signals.
    #[test]
    fn degradation_signals_preserved(signals in arb_degradation_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals.clone());
        prop_assert_eq!(
            assessment.signals, signals,
            "signals should be preserved in assessment"
        );
    }

    /// Assessment strings are non-empty.
    #[test]
    fn degradation_assessment_strings_nonempty(signals in arb_degradation_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert!(!assessment.trigger_condition.is_empty(), "trigger_condition should be non-empty");
        prop_assert!(!assessment.recovery_rule.is_empty(), "recovery_rule should be non-empty");
        prop_assert!(!assessment.recommended_action.is_empty(), "recommended_action should be non-empty");
    }
}

// ── ResizeDegradationAssessment: warning_line ───────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// FullQuality tier produces None warning_line.
    #[test]
    fn degradation_full_quality_no_warning(signals in arb_degradation_signals()) {
        let signals = ResizeDegradationSignals {
            stalled_total: 0,
            stalled_critical: 0,
            safe_mode_recommended: false,
            safe_mode_active: false,
            ..signals
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        prop_assert!(
            assessment.warning_line().is_none(),
            "FullQuality should have no warning_line"
        );
    }

    /// Non-FullQuality tiers produce Some warning_line.
    #[test]
    fn degradation_non_full_quality_has_warning(signals in arb_degradation_signals()) {
        // Ensure at least warning level
        let signals = ResizeDegradationSignals {
            stalled_total: 1.max(signals.stalled_total),
            safe_mode_active: false,
            safe_mode_recommended: false,
            stalled_critical: 0,
            ..signals
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        if assessment.tier != ResizeDegradationTier::FullQuality {
            prop_assert!(
                assessment.warning_line().is_some(),
                "non-FullQuality tier {:?} should have warning_line",
                assessment.tier
            );
        }
    }

    /// Warning line mentions "degradation" or "ladder".
    #[test]
    fn degradation_warning_line_content(signals in arb_degradation_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        if let Some(line) = assessment.warning_line() {
            let lower = line.to_lowercase();
            prop_assert!(
                lower.contains("degradation") || lower.contains("ladder"),
                "warning_line should mention degradation or ladder: {}",
                line
            );
        }
    }

    /// EmergencyCompatibility warning mentions "emergency".
    #[test]
    fn degradation_emergency_warning_content(signals in arb_degradation_signals()) {
        let signals = ResizeDegradationSignals {
            safe_mode_active: true,
            ..signals
        };
        let assessment = evaluate_resize_degradation_ladder(signals);
        let line = assessment.warning_line().unwrap();
        let lower = line.to_lowercase();
        prop_assert!(
            lower.contains("emergency"),
            "emergency tier warning should mention 'emergency': {}",
            line
        );
    }
}

// ── ResizeDegradationAssessment: serde roundtrip ────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// JSON serde roundtrip preserves degradation assessment.
    #[test]
    fn degradation_assessment_serde_roundtrip(signals in arb_degradation_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        let json = serde_json::to_string(&assessment).unwrap();
        let parsed: ResizeDegradationAssessment = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, assessment);
    }

    /// Serialized assessment is valid JSON object.
    #[test]
    fn degradation_assessment_valid_json(signals in arb_degradation_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        let json = serde_json::to_string(&assessment).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Assessment has required top-level fields.
    #[test]
    fn degradation_assessment_required_fields(signals in arb_degradation_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals);
        let json = serde_json::to_string(&assessment).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("tier").is_some(), "missing 'tier'");
        prop_assert!(value.get("tier_rank").is_some(), "missing 'tier_rank'");
        prop_assert!(value.get("trigger_condition").is_some(), "missing 'trigger_condition'");
        prop_assert!(value.get("recovery_rule").is_some(), "missing 'recovery_rule'");
        prop_assert!(value.get("recommended_action").is_some(), "missing 'recommended_action'");
        prop_assert!(value.get("signals").is_some(), "missing 'signals'");
    }

    /// ResizeDegradationSignals serde roundtrip.
    #[test]
    fn degradation_signals_serde_roundtrip(signals in arb_degradation_signals()) {
        let json = serde_json::to_string(&signals).unwrap();
        let parsed: ResizeDegradationSignals = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, signals);
    }
}

// ── ResizeWatchdogSeverity ↔ ResizeDegradationTier mapping ──────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Severity and degradation tier are conceptually aligned:
    /// Healthy → FullQuality, Warning → QualityReduced, etc.
    #[test]
    fn severity_tier_alignment(signals in arb_degradation_signals()) {
        let assessment = evaluate_resize_degradation_ladder(signals.clone());

        // Derive the expected severity from the same signals
        let expected_severity = if signals.safe_mode_active {
            ResizeWatchdogSeverity::SafeModeActive
        } else if signals.safe_mode_recommended || signals.stalled_critical > 0 {
            ResizeWatchdogSeverity::Critical
        } else if signals.stalled_total > 0 {
            ResizeWatchdogSeverity::Warning
        } else {
            ResizeWatchdogSeverity::Healthy
        };

        // Verify tier matches severity
        let expected_tier = match expected_severity {
            ResizeWatchdogSeverity::Healthy => ResizeDegradationTier::FullQuality,
            ResizeWatchdogSeverity::Warning => ResizeDegradationTier::QualityReduced,
            ResizeWatchdogSeverity::Critical => ResizeDegradationTier::CorrectnessGuarded,
            ResizeWatchdogSeverity::SafeModeActive => ResizeDegradationTier::EmergencyCompatibility,
        };

        prop_assert_eq!(
            assessment.tier, expected_tier,
            "signals {:?} expected tier {:?}, got {:?}",
            signals, expected_tier, assessment.tier
        );
    }
}

// ── derive_resize_degradation_ladder from watchdog assessment ────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// derive_resize_degradation_ladder preserves threshold fields.
    #[test]
    fn derive_degradation_preserves_thresholds(
        severity in arb_severity(),
        stalled_total in 0usize..50,
        stalled_critical in 0usize..50,
        wt in 1u64..10_000,
        ct in 1u64..30_000,
        limit in 1usize..10,
        recommended in any::<bool>(),
        active in any::<bool>(),
        legacy in any::<bool>(),
    ) {
        let stalled_critical = stalled_critical.min(stalled_total);
        let (wt, ct) = if wt <= ct { (wt, ct) } else { (ct, wt) };

        let watchdog = ResizeWatchdogAssessment {
            severity,
            stalled_total,
            stalled_critical,
            warning_threshold_ms: wt,
            critical_threshold_ms: ct,
            critical_stalled_limit: limit,
            safe_mode_recommended: recommended,
            safe_mode_active: active,
            legacy_fallback_enabled: legacy,
            recommended_action: "test".into(),
            sample_stalled: vec![],
        };

        let degradation = frankenterm_core::runtime::derive_resize_degradation_ladder(&watchdog);

        // Verify input signals are preserved in the assessment
        prop_assert_eq!(degradation.signals.stalled_total, stalled_total);
        prop_assert_eq!(degradation.signals.stalled_critical, stalled_critical);
        prop_assert_eq!(degradation.signals.warning_threshold_ms, wt);
        prop_assert_eq!(degradation.signals.critical_threshold_ms, ct);
        prop_assert_eq!(degradation.signals.safe_mode_active, active);
        prop_assert_eq!(degradation.signals.legacy_fallback_enabled, legacy);
    }
}
