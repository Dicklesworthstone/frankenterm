//! Property-based tests for storage_telemetry module.
//!
//! Verifies storage pipeline telemetry invariants:
//! - StorageHealthTier classify monotonicity: higher ratio → same or higher tier
//! - StorageHealthTier classify boundary: exact threshold values produce correct tier
//! - Degraded flag always → Black tier regardless of ratio
//! - Tier ordering: Green < Yellow < Red < Black
//! - Counter monotonicity: record_append/flush/checkpoint only increase counters
//! - Counter additivity: batch sizes sum to total_events_appended
//! - ErrorCounts total == sum of all fields
//! - SLO evaluation correctness: Met/Breached/Unknown match thresholds
//! - Diagnostic tier-recommendation consistency
//! - Serde roundtrip: StorageHealthTier, SloStatus, StorageTelemetryConfig, ErrorCounts
//! - Config threshold ordering: yellow < red < black
//! - Remediation coverage: every error class has non-empty remediation

use proptest::prelude::*;

use frankenterm_core::recorder_storage::{
    CheckpointCommitOutcome, RecorderBackendKind, RecorderStorageErrorClass, RecorderStorageHealth,
};
use frankenterm_core::storage_telemetry::{
    ErrorCounts, SloStatus, StorageHealthTier, StorageTelemetry, StorageTelemetryConfig, diagnose,
    remediation_for_error,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_health_tier() -> impl Strategy<Value = StorageHealthTier> {
    prop_oneof![
        Just(StorageHealthTier::Green),
        Just(StorageHealthTier::Yellow),
        Just(StorageHealthTier::Red),
        Just(StorageHealthTier::Black),
    ]
}

fn arb_slo_status() -> impl Strategy<Value = SloStatus> {
    prop_oneof![
        Just(SloStatus::Met),
        Just(SloStatus::Breached),
        Just(SloStatus::Unknown),
    ]
}

fn arb_error_class() -> impl Strategy<Value = RecorderStorageErrorClass> {
    prop_oneof![
        Just(RecorderStorageErrorClass::Overload),
        Just(RecorderStorageErrorClass::Retryable),
        Just(RecorderStorageErrorClass::TerminalData),
        Just(RecorderStorageErrorClass::TerminalConfig),
        Just(RecorderStorageErrorClass::Corruption),
        Just(RecorderStorageErrorClass::DependencyUnavailable),
    ]
}

fn arb_checkpoint_outcome() -> impl Strategy<Value = CheckpointCommitOutcome> {
    prop_oneof![
        Just(CheckpointCommitOutcome::Advanced),
        Just(CheckpointCommitOutcome::NoopAlreadyAdvanced),
        Just(CheckpointCommitOutcome::RejectedOutOfOrder),
    ]
}

/// Valid threshold triple: 0 < yellow < red < black <= 1.0
fn arb_thresholds() -> impl Strategy<Value = [f64; 3]> {
    (0.05f64..=0.45, 0.05f64..=0.45, 0.05f64..=0.45).prop_map(|(a, b, c)| {
        let mut vals = [a, a + b, a + b + c];
        // Clamp to [0, 1]
        for v in &mut vals {
            *v = v.min(1.0);
        }
        vals
    })
}

fn arb_error_counts() -> impl Strategy<Value = ErrorCounts> {
    (0u64..=100, 0u64..=100, 0u64..=100, 0u64..=100).prop_map(
        |(overload, retryable, terminal_data, corruption)| ErrorCounts {
            overload,
            retryable,
            terminal_data,
            corruption,
        },
    )
}

// ────────────────────────────────────────────────────────────────────
// Properties
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // ────────────────────────────────────────────────────────────────
    // StorageHealthTier::classify
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn classify_monotonic_in_ratio(
        thresholds in arb_thresholds(),
        ratio_a in 0.0f64..=1.0,
        ratio_b in 0.0f64..=1.0
    ) {
        // Higher ratio → same or higher tier (when not degraded).
        let tier_a = StorageHealthTier::classify(ratio_a, false, &thresholds);
        let tier_b = StorageHealthTier::classify(ratio_b, false, &thresholds);
        if ratio_a <= ratio_b {
            prop_assert!(tier_a <= tier_b,
                "classify({}) = {:?} should be <= classify({}) = {:?} with thresholds {:?}",
                ratio_a, tier_a, ratio_b, tier_b, thresholds);
        }
    }

    #[test]
    fn classify_degraded_always_black(
        ratio in 0.0f64..=1.0,
        thresholds in arb_thresholds()
    ) {
        let tier = StorageHealthTier::classify(ratio, true, &thresholds);
        prop_assert_eq!(tier, StorageHealthTier::Black,
            "Degraded must always be Black, got {:?} at ratio {}", tier, ratio);
    }

    #[test]
    fn classify_zero_ratio_not_degraded_is_green(thresholds in arb_thresholds()) {
        let tier = StorageHealthTier::classify(0.0, false, &thresholds);
        prop_assert_eq!(tier, StorageHealthTier::Green,
            "Zero ratio non-degraded must be Green, got {:?}", tier);
    }

    #[test]
    fn classify_at_exact_thresholds(thresholds in arb_thresholds()) {
        // Below yellow → Green
        if thresholds[0] > 0.01 {
            let tier = StorageHealthTier::classify(thresholds[0] - 0.01, false, &thresholds);
            prop_assert_eq!(tier, StorageHealthTier::Green);
        }

        // At yellow threshold → Yellow
        let tier = StorageHealthTier::classify(thresholds[0], false, &thresholds);
        prop_assert!(tier >= StorageHealthTier::Yellow,
            "At yellow threshold {} should be >= Yellow, got {:?}", thresholds[0], tier);

        // At red threshold → Red or higher
        let tier = StorageHealthTier::classify(thresholds[1], false, &thresholds);
        prop_assert!(tier >= StorageHealthTier::Red,
            "At red threshold {} should be >= Red, got {:?}", thresholds[1], tier);

        // At black threshold → Black
        let tier = StorageHealthTier::classify(thresholds[2], false, &thresholds);
        prop_assert_eq!(tier, StorageHealthTier::Black,
            "At black threshold {} should be Black, got {:?}", thresholds[2], tier);
    }

    // ────────────────────────────────────────────────────────────────
    // Tier ordering
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn tier_ordering_is_total(a in arb_health_tier(), b in arb_health_tier()) {
        // Total order: either a <= b or b <= a.
        prop_assert!(a <= b || b <= a,
            "{:?} and {:?} must be totally ordered", a, b);
    }

    #[test]
    fn tier_ordering_consistent_with_discriminant(a in arb_health_tier(), b in arb_health_tier()) {
        let disc_a = a as u8;
        let disc_b = b as u8;
        prop_assert_eq!(a.cmp(&b), disc_a.cmp(&disc_b),
            "Tier ordering must match discriminant ordering");
    }

    // ────────────────────────────────────────────────────────────────
    // Counter monotonicity
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn counters_monotonically_increase(
        batch_sizes in proptest::collection::vec(1usize..=50, 1..=20),
        latencies in proptest::collection::vec(10.0f64..=10000.0, 1..=20)
    ) {
        let telem = StorageTelemetry::with_defaults();
        let n = batch_sizes.len().min(latencies.len());

        let mut prev_events = 0u64;
        let mut prev_batches = 0u64;

        for i in 0..n {
            telem.record_append(latencies[i], batch_sizes[i], (batch_sizes[i] as u64) * 256, false);

            let snap = telem.snapshot();
            prop_assert!(snap.total_events_appended >= prev_events,
                "Events counter decreased: {} -> {}", prev_events, snap.total_events_appended);
            prop_assert!(snap.total_batches >= prev_batches,
                "Batches counter decreased: {} -> {}", prev_batches, snap.total_batches);

            prev_events = snap.total_events_appended;
            prev_batches = snap.total_batches;
        }
    }

    #[test]
    fn counter_additivity(
        batch_sizes in proptest::collection::vec(1usize..=100, 1..=30)
    ) {
        let telem = StorageTelemetry::with_defaults();
        let expected_total: usize = batch_sizes.iter().sum();

        for size in &batch_sizes {
            telem.record_append(100.0, *size, (*size as u64) * 256, false);
        }

        let snap = telem.snapshot();
        prop_assert_eq!(snap.total_events_appended, expected_total as u64,
            "Total events {} should equal sum of batches {}", snap.total_events_appended, expected_total);
        prop_assert_eq!(snap.total_batches, batch_sizes.len() as u64);
    }

    #[test]
    fn flush_counter_increments(num_flushes in 1usize..=50) {
        let telem = StorageTelemetry::with_defaults();

        for _ in 0..num_flushes {
            telem.record_flush(500.0);
        }

        let snap = telem.snapshot();
        prop_assert_eq!(snap.total_flushes, num_flushes as u64);
    }

    #[test]
    fn checkpoint_counter_increments(
        outcomes in proptest::collection::vec(arb_checkpoint_outcome(), 1..=30)
    ) {
        let telem = StorageTelemetry::with_defaults();

        for outcome in &outcomes {
            telem.record_checkpoint(200.0, *outcome);
        }

        let snap = telem.snapshot();
        prop_assert_eq!(snap.total_checkpoints, outcomes.len() as u64);
    }

    // ────────────────────────────────────────────────────────────────
    // Error counts
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn error_counts_total_is_sum(ec in arb_error_counts()) {
        let total = ec.total();
        let expected = ec.overload + ec.retryable + ec.terminal_data + ec.corruption;
        prop_assert_eq!(total, expected,
            "total() {} should equal field sum {}", total, expected);
    }

    #[test]
    fn error_recording_increments_correct_counter(
        errors in proptest::collection::vec(arb_error_class(), 1..=30)
    ) {
        let telem = StorageTelemetry::with_defaults();

        let mut expected_overload = 0u64;
        let mut expected_retryable = 0u64;
        let mut expected_terminal = 0u64;
        let mut expected_corruption = 0u64;

        for class in &errors {
            telem.record_error(*class);
            match class {
                RecorderStorageErrorClass::Overload => expected_overload += 1,
                RecorderStorageErrorClass::Retryable => expected_retryable += 1,
                RecorderStorageErrorClass::TerminalData
                | RecorderStorageErrorClass::TerminalConfig => expected_terminal += 1,
                RecorderStorageErrorClass::Corruption => expected_corruption += 1,
                RecorderStorageErrorClass::DependencyUnavailable => expected_retryable += 1,
            }
        }

        let snap = telem.snapshot();
        prop_assert_eq!(snap.errors.overload, expected_overload);
        prop_assert_eq!(snap.errors.retryable, expected_retryable);
        prop_assert_eq!(snap.errors.terminal_data, expected_terminal);
        prop_assert_eq!(snap.errors.corruption, expected_corruption);
    }

    // ────────────────────────────────────────────────────────────────
    // SLO evaluation
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn slo_unknown_with_no_data(
        slo_target in 100.0f64..=100_000.0
    ) {
        let config = StorageTelemetryConfig {
            slo_append_p95_us: slo_target,
            slo_flush_p95_us: slo_target,
            ..StorageTelemetryConfig::default()
        };
        let telem = StorageTelemetry::new(config);
        let snap = telem.snapshot();

        prop_assert_eq!(snap.slo_append_p95, SloStatus::Unknown);
        prop_assert_eq!(snap.slo_flush_p95, SloStatus::Unknown);
    }

    #[test]
    fn slo_met_when_all_latencies_below_target(
        latency in 1.0f64..=100.0,
        count in 10usize..=50
    ) {
        let config = StorageTelemetryConfig {
            slo_append_p95_us: latency * 10.0, // target is 10x the actual latency
            ..StorageTelemetryConfig::default()
        };
        let telem = StorageTelemetry::new(config);

        for _ in 0..count {
            telem.record_append(latency, 1, 256, false);
        }

        let snap = telem.snapshot();
        prop_assert_eq!(snap.slo_append_p95, SloStatus::Met,
            "SLO should be Met when all latencies {} < target {}", latency, latency * 10.0);
    }

    // ────────────────────────────────────────────────────────────────
    // Diagnostic tier-recommendation consistency
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn diagnose_green_has_no_recommendation(
        events in 0u64..=100
    ) {
        let telem = StorageTelemetry::with_defaults();
        // Don't set health → defaults to Green.
        for _ in 0..events {
            telem.record_append(100.0, 1, 256, false);
        }

        let snap = telem.snapshot();
        let diag = diagnose(&snap);

        if diag.tier == StorageHealthTier::Green {
            prop_assert!(diag.recommendation.is_none(),
                "Green tier should have no recommendation");
        }
    }

    #[test]
    fn diagnose_non_green_has_recommendation(
        ratio in 0.5f64..=1.0,
        degraded in prop::bool::ANY
    ) {
        let telem = StorageTelemetry::with_defaults();
        let health = RecorderStorageHealth {
            backend: RecorderBackendKind::AppendLog,
            degraded,
            queue_depth: (ratio * 100.0) as usize,
            queue_capacity: 100,
            latest_offset: None,
            last_error: if degraded {
                Some("test".to_string())
            } else {
                None
            },
        };
        telem.update_health(health);

        let snap = telem.snapshot();
        let diag = diagnose(&snap);

        if diag.tier != StorageHealthTier::Green {
            prop_assert!(diag.recommendation.is_some(),
                "Non-green tier {:?} should have a recommendation", diag.tier);
        }
    }

    // ────────────────────────────────────────────────────────────────
    // Remediation coverage
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn remediation_is_non_empty(class in arb_error_class()) {
        let msg = remediation_for_error(class);
        prop_assert!(!msg.is_empty(),
            "Remediation for {:?} must be non-empty", class);
    }

    // ────────────────────────────────────────────────────────────────
    // Serde roundtrip
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn health_tier_serde_roundtrip(tier in arb_health_tier()) {
        let json = serde_json::to_string(&tier).unwrap();
        let parsed: StorageHealthTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, parsed);
    }

    #[test]
    fn slo_status_serde_roundtrip(status in arb_slo_status()) {
        let json = serde_json::to_string(&status).unwrap();
        let parsed: SloStatus = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(status, parsed);
    }

    #[test]
    fn error_counts_serde_roundtrip(ec in arb_error_counts()) {
        let json = serde_json::to_string(&ec).unwrap();
        let parsed: ErrorCounts = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ec.overload, parsed.overload);
        prop_assert_eq!(ec.retryable, parsed.retryable);
        prop_assert_eq!(ec.terminal_data, parsed.terminal_data);
        prop_assert_eq!(ec.corruption, parsed.corruption);
        prop_assert_eq!(ec.total(), parsed.total());
    }

    #[test]
    fn config_serde_roundtrip(
        histogram_max in 100usize..=5000,
        half_life in 1000.0f64..=30000.0,
        slo_append in 1000.0f64..=500_000.0,
        slo_flush in 1000.0f64..=500_000.0,
        thresholds in arb_thresholds()
    ) {
        let config = StorageTelemetryConfig {
            histogram_max_samples: histogram_max,
            tier_thresholds: thresholds,
            rate_ewma_half_life_ms: half_life,
            slo_append_p95_us: slo_append,
            slo_flush_p95_us: slo_flush,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: StorageTelemetryConfig = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(config.histogram_max_samples, parsed.histogram_max_samples);
        // Float comparison with tolerance.
        prop_assert!((config.rate_ewma_half_life_ms - parsed.rate_ewma_half_life_ms).abs() < 0.01);
        prop_assert!((config.slo_append_p95_us - parsed.slo_append_p95_us).abs() < 0.01);
        prop_assert!((config.slo_flush_p95_us - parsed.slo_flush_p95_us).abs() < 0.01);
    }

    // ────────────────────────────────────────────────────────────────
    // Health update changes tier
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn health_update_changes_tier_consistently(
        depth in 0usize..=100,
        capacity in 1usize..=100
    ) {
        let telem = StorageTelemetry::with_defaults();
        let health = RecorderStorageHealth {
            backend: RecorderBackendKind::AppendLog,
            degraded: false,
            queue_depth: depth.min(capacity),
            queue_capacity: capacity,
            latest_offset: None,
            last_error: None,
        };
        telem.update_health(health);

        let tier = telem.current_tier();
        let ratio = depth.min(capacity) as f64 / capacity as f64;
        let expected = StorageHealthTier::classify(
            ratio,
            false,
            &StorageTelemetryConfig::default().tier_thresholds,
        );

        prop_assert_eq!(tier, expected,
            "Tier {:?} should match classify({}) = {:?}", tier, ratio, expected);
    }

    // ────────────────────────────────────────────────────────────────
    // Snapshot consistency
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn snapshot_error_counts_match_recordings(
        errors in proptest::collection::vec(arb_error_class(), 0..=20)
    ) {
        let telem = StorageTelemetry::with_defaults();
        for class in &errors {
            telem.record_error(*class);
        }

        let snap = telem.snapshot();
        let total_recorded = errors.len() as u64;
        let total_snapshot = snap.errors.total();

        prop_assert_eq!(total_snapshot, total_recorded,
            "Snapshot error total {} should match recorded count {}", total_snapshot, total_recorded);
    }

    #[test]
    fn snapshot_timestamp_is_reasonable(
        num_appends in 0usize..=5
    ) {
        let telem = StorageTelemetry::with_defaults();
        for _ in 0..num_appends {
            telem.record_append(100.0, 1, 256, false);
        }

        let snap = telem.snapshot();
        // Timestamp should be a reasonable epoch ms (after 2020).
        prop_assert!(snap.timestamp_ms > 1_577_836_800_000,
            "Timestamp {} should be a recent epoch ms", snap.timestamp_ms);
    }

    // ────────────────────────────────────────────────────────────────
    // Default config sanity
    // ────────────────────────────────────────────────────────────────

    #[test]
    fn default_config_thresholds_ordered(_dummy in 0..1u8) {
        let cfg = StorageTelemetryConfig::default();
        prop_assert!(cfg.tier_thresholds[0] < cfg.tier_thresholds[1],
            "Yellow {} must be < Red {}", cfg.tier_thresholds[0], cfg.tier_thresholds[1]);
        prop_assert!(cfg.tier_thresholds[1] < cfg.tier_thresholds[2],
            "Red {} must be < Black {}", cfg.tier_thresholds[1], cfg.tier_thresholds[2]);
        prop_assert!(cfg.slo_append_p95_us > 0.0);
        prop_assert!(cfg.slo_flush_p95_us > 0.0);
        prop_assert!(cfg.rate_ewma_half_life_ms > 0.0);
    }
}
