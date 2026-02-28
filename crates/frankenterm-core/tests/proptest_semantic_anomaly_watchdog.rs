//! Property-based tests for semantic_anomaly_watchdog.rs.
//!
//! Covers serde roundtrips for WatchdogConfig, WatchdogMetricsSnapshot, and
//! SemanticAnomalyEvent; WatchdogHandle segment filtering (min/max segment
//! bytes, shedding under capacity pressure); metrics accounting consistency;
//! and config default invariants.

use frankenterm_core::semantic_anomaly::{
    ConformalAnomalyConfig, ConformalShock, EntropyGateConfig,
};
use frankenterm_core::semantic_anomaly_watchdog::{
    SemanticAnomalyEvent, WatchdogConfig, WatchdogMetricsSnapshot,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_entropy_gate_config() -> impl Strategy<Value = EntropyGateConfig> {
    (
        (0..=80u64).prop_map(|v| v as f64 / 10.0), // 0.0..8.0
        1..=64usize,
        any::<bool>(),
    )
        .prop_map(|(min_entropy, min_bytes, enabled)| EntropyGateConfig {
            min_entropy_bits_per_byte: min_entropy,
            min_segment_bytes: min_bytes,
            enabled,
        })
}

fn arb_conformal_config() -> impl Strategy<Value = ConformalAnomalyConfig> {
    (
        (1..=50u64).prop_map(|v| v as f64 / 100.0), // alpha: 0.01..0.50
        10..=500usize,
        (1..=50u64).prop_map(|v| v as f32 / 100.0), // centroid_alpha: 0.01..0.50
        1..=50usize,
    )
        .prop_map(
            |(alpha, calibration_window, centroid_alpha, min_calibration)| ConformalAnomalyConfig {
                alpha,
                calibration_window,
                centroid_alpha,
                min_calibration,
            },
        )
}

fn arb_watchdog_config() -> impl Strategy<Value = WatchdogConfig> {
    (
        1..=512usize,
        1..=64usize,
        1..=100u64,
        arb_entropy_gate_config(),
        arb_conformal_config(),
        1..=64usize,
        64..=131072usize,
    )
        .prop_map(
            |(
                queue_capacity,
                batch_size,
                batch_timeout_ms,
                entropy_gate,
                conformal,
                min_segment_bytes,
                max_segment_bytes,
            )| {
                let (min_sb, max_sb) = if min_segment_bytes <= max_segment_bytes {
                    (min_segment_bytes, max_segment_bytes)
                } else {
                    (max_segment_bytes, min_segment_bytes)
                };
                WatchdogConfig {
                    queue_capacity,
                    batch_size,
                    batch_timeout_ms,
                    entropy_gate,
                    conformal,
                    min_segment_bytes: min_sb,
                    max_segment_bytes: max_sb,
                }
            },
        )
}

fn arb_metrics_snapshot() -> impl Strategy<Value = WatchdogMetricsSnapshot> {
    (
        0..=10_000u64,
        0..=1_000u64,
        0..=10_000u64,
        0..=5_000u64,
        0..=5_000u64,
        0..=100u64,
        0..=1_000u64,
        (0..=1600u64).prop_map(|v| v as f64 / 100.0), // avg_batch_fill: 0..16
        0..=1_000u64,
        0..=1_000u64,
    )
        .prop_map(
            |(
                submitted,
                shed,
                processed,
                entropy_skipped,
                embedded,
                anomalies,
                batches,
                avg_fill,
                too_short,
                truncated,
            )| {
                WatchdogMetricsSnapshot {
                    segments_submitted: submitted,
                    segments_shed: shed,
                    segments_processed: processed,
                    segments_entropy_skipped: entropy_skipped,
                    segments_embedded: embedded,
                    anomalies_detected: anomalies,
                    batches_processed: batches,
                    avg_batch_fill: avg_fill,
                    segments_too_short: too_short,
                    segments_truncated: truncated,
                }
            },
        )
}

fn arb_conformal_shock() -> impl Strategy<Value = ConformalShock> {
    (
        (0..=1000u64).prop_map(|v| v as f32 / 1000.0), // distance: 0.0..1.0
        (0..=1000u64).prop_map(|v| v as f64 / 1000.0), // p_value: 0.0..1.0
        (1..=50u64).prop_map(|v| v as f64 / 100.0),    // alpha: 0.01..0.50
        1..=500usize,
        (0..=1000u64).prop_map(|v| v as f32 / 1000.0), // median: 0.0..1.0
    )
        .prop_map(
            |(distance, p_value, alpha, calibration_count, calibration_median)| ConformalShock {
                distance,
                p_value,
                alpha,
                calibration_count,
                calibration_median,
            },
        )
}

fn arb_anomaly_event() -> impl Strategy<Value = SemanticAnomalyEvent> {
    (any::<u64>(), arb_conformal_shock(), 1..=65536usize).prop_map(
        |(pane_id, shock, segment_len)| SemanticAnomalyEvent {
            pane_id,
            shock,
            segment_len,
        },
    )
}

// ── WatchdogConfig serde ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 1. WatchdogConfig serde roundtrip
    #[test]
    fn config_serde_roundtrip(config in arb_watchdog_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let restored: WatchdogConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.queue_capacity, config.queue_capacity);
        prop_assert_eq!(restored.batch_size, config.batch_size);
        prop_assert_eq!(restored.batch_timeout_ms, config.batch_timeout_ms);
        prop_assert_eq!(restored.min_segment_bytes, config.min_segment_bytes);
        prop_assert_eq!(restored.max_segment_bytes, config.max_segment_bytes);
    }

    // 2. Config default has sensible values
    #[test]
    fn config_default_valid(_seed in 0..10u32) {
        let config = WatchdogConfig::default();
        prop_assert!(config.queue_capacity > 0);
        prop_assert!(config.batch_size > 0);
        prop_assert!(config.batch_timeout_ms > 0);
        prop_assert!(config.min_segment_bytes <= config.max_segment_bytes);
    }

    // 3. Config debug output is non-empty
    #[test]
    fn config_debug_non_empty(config in arb_watchdog_config()) {
        let debug = format!("{:?}", config);
        prop_assert!(!debug.is_empty());
        prop_assert!(debug.contains("queue_capacity"));
    }
}

// ── WatchdogMetricsSnapshot serde ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 4. Metrics snapshot serde roundtrip
    #[test]
    fn metrics_snapshot_serde_roundtrip(snap in arb_metrics_snapshot()) {
        let json = serde_json::to_string(&snap).unwrap();
        let restored: WatchdogMetricsSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.segments_submitted, snap.segments_submitted);
        prop_assert_eq!(restored.segments_shed, snap.segments_shed);
        prop_assert_eq!(restored.segments_processed, snap.segments_processed);
        prop_assert_eq!(restored.segments_entropy_skipped, snap.segments_entropy_skipped);
        prop_assert_eq!(restored.segments_embedded, snap.segments_embedded);
        prop_assert_eq!(restored.anomalies_detected, snap.anomalies_detected);
        prop_assert_eq!(restored.batches_processed, snap.batches_processed);
        prop_assert_eq!(restored.segments_too_short, snap.segments_too_short);
        prop_assert_eq!(restored.segments_truncated, snap.segments_truncated);
        prop_assert!((restored.avg_batch_fill - snap.avg_batch_fill).abs() < 1e-10);
    }

    // 5. Metrics debug output contains key field names
    #[test]
    fn metrics_debug_fields(snap in arb_metrics_snapshot()) {
        let debug = format!("{:?}", snap);
        prop_assert!(debug.contains("segments_submitted"));
        prop_assert!(debug.contains("anomalies_detected"));
    }
}

// ── SemanticAnomalyEvent serde ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 6. SemanticAnomalyEvent serde roundtrip
    #[test]
    fn anomaly_event_serde_roundtrip(event in arb_anomaly_event()) {
        let json = serde_json::to_string(&event).unwrap();
        let restored: SemanticAnomalyEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(restored.pane_id, event.pane_id);
        prop_assert_eq!(restored.segment_len, event.segment_len);
        prop_assert!((restored.shock.p_value - event.shock.p_value).abs() < 1e-10);
        prop_assert!((restored.shock.distance - event.shock.distance).abs() < 1e-6);
        prop_assert!((restored.shock.alpha - event.shock.alpha).abs() < 1e-10);
        prop_assert_eq!(restored.shock.calibration_count, event.shock.calibration_count);
    }

    // 7. SemanticAnomalyEvent debug contains pane_id
    #[test]
    fn anomaly_event_debug(event in arb_anomaly_event()) {
        let debug = format!("{:?}", event);
        prop_assert!(debug.contains("pane_id"));
        prop_assert!(debug.contains("shock"));
    }
}

// ── ConformalShock serde ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    // 8. ConformalShock serde roundtrip
    #[test]
    fn conformal_shock_serde_roundtrip(shock in arb_conformal_shock()) {
        let json = serde_json::to_string(&shock).unwrap();
        let restored: ConformalShock = serde_json::from_str(&json).unwrap();
        prop_assert!((restored.distance - shock.distance).abs() < 1e-6);
        prop_assert!((restored.p_value - shock.p_value).abs() < 1e-10);
        prop_assert!((restored.alpha - shock.alpha).abs() < 1e-10);
        prop_assert_eq!(restored.calibration_count, shock.calibration_count);
        prop_assert!((restored.calibration_median - shock.calibration_median).abs() < 1e-6);
    }

    // 9. ConformalShock equality (same values → equal)
    #[test]
    fn conformal_shock_equality(shock in arb_conformal_shock()) {
        let clone = shock.clone();
        prop_assert_eq!(clone, shock);
    }
}

// ── EntropyGateConfig serde ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 10. EntropyGateConfig serde roundtrip
    #[test]
    fn entropy_gate_config_serde_roundtrip(config in arb_entropy_gate_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let restored: EntropyGateConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((restored.min_entropy_bits_per_byte - config.min_entropy_bits_per_byte).abs() < 1e-10);
        prop_assert_eq!(restored.min_segment_bytes, config.min_segment_bytes);
        prop_assert_eq!(restored.enabled, config.enabled);
    }
}

// ── ConformalAnomalyConfig serde ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    // 11. ConformalAnomalyConfig serde roundtrip
    #[test]
    fn conformal_anomaly_config_serde_roundtrip(config in arb_conformal_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let restored: ConformalAnomalyConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((restored.alpha - config.alpha).abs() < 1e-10);
        prop_assert_eq!(restored.calibration_window, config.calibration_window);
        prop_assert!((restored.centroid_alpha - config.centroid_alpha).abs() < 1e-6);
        prop_assert_eq!(restored.min_calibration, config.min_calibration);
    }
}

// ── Watchdog start/handle lifecycle tests ────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8))]

    // 12. Watchdog processes submitted segments (integration-style proptest)
    #[test]
    fn watchdog_processes_diverse_segments(
        n_segments in 5..=20usize,
        pane_id in 1..=100u64,
    ) {
        use frankenterm_core::semantic_anomaly_watchdog::SemanticAnomalyWatchdog;

        let config = WatchdogConfig {
            queue_capacity: 64,
            batch_size: 8,
            batch_timeout_ms: 5,
            min_segment_bytes: 2,
            max_segment_bytes: 1024,
            entropy_gate: EntropyGateConfig {
                min_entropy_bits_per_byte: 0.5,
                min_segment_bytes: 2,
                enabled: true,
            },
            conformal: ConformalAnomalyConfig {
                min_calibration: 5,
                calibration_window: 50,
                alpha: 0.05,
                centroid_alpha: 0.1,
            },
        };

        fn mock_embed(data: &[u8]) -> Vec<f32> {
            let mut v = vec![0.0f32; 8];
            for (i, &b) in data.iter().take(8).enumerate() {
                v[i] = b as f32 / 255.0;
            }
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > f32::EPSILON {
                for x in &mut v {
                    *x /= norm;
                }
            }
            v
        }

        let watchdog = SemanticAnomalyWatchdog::start(config, mock_embed, None);
        let handle = watchdog.handle();

        for i in 0..n_segments {
            let data: Vec<u8> = (0..64).map(|j| ((i * 17 + j * 31) % 256) as u8).collect();
            handle.observe_segment(pane_id, &data);
        }

        std::thread::sleep(std::time::Duration::from_millis(50));
        let snap = watchdog.metrics();
        prop_assert_eq!(snap.segments_submitted, n_segments as u64);
        prop_assert!(snap.segments_processed > 0);

        watchdog.shutdown();
    }

    // 13. Segments shorter than min_segment_bytes are rejected
    #[test]
    fn watchdog_rejects_short_segments(
        min_bytes in 2..=16usize,
        data_len in 1..=16usize,
    ) {
        use frankenterm_core::semantic_anomaly_watchdog::SemanticAnomalyWatchdog;

        let config = WatchdogConfig {
            queue_capacity: 32,
            batch_size: 4,
            batch_timeout_ms: 5,
            min_segment_bytes: min_bytes,
            max_segment_bytes: 1024,
            entropy_gate: EntropyGateConfig::default(),
            conformal: ConformalAnomalyConfig::default(),
        };

        fn noop_embed(_data: &[u8]) -> Vec<f32> { vec![0.0; 8] }

        let watchdog = SemanticAnomalyWatchdog::start(config, noop_embed, None);
        let handle = watchdog.handle();

        let data = vec![42u8; data_len];
        let accepted = handle.observe_segment(1, &data);

        let snap = watchdog.metrics();
        if data_len < min_bytes {
            prop_assert!(!accepted, "segment shorter than min should be rejected");
            prop_assert_eq!(snap.segments_too_short, 1);
            prop_assert_eq!(snap.segments_submitted, 0);
        } else {
            prop_assert!(accepted, "segment >= min should be accepted");
            prop_assert_eq!(snap.segments_submitted, 1);
        }

        watchdog.shutdown();
    }

    // 14. Shedding occurs when queue is full
    #[test]
    fn watchdog_sheds_under_pressure(queue_cap in 2..=8usize) {
        use frankenterm_core::semantic_anomaly_watchdog::SemanticAnomalyWatchdog;

        let config = WatchdogConfig {
            queue_capacity: queue_cap,
            batch_size: 4,
            batch_timeout_ms: 200, // Slow batching to fill queue
            min_segment_bytes: 2,
            max_segment_bytes: 1024,
            entropy_gate: EntropyGateConfig::default(),
            conformal: ConformalAnomalyConfig::default(),
        };

        fn slow_embed(_data: &[u8]) -> Vec<f32> {
            std::thread::sleep(std::time::Duration::from_millis(10));
            vec![0.0; 8]
        }

        let watchdog = SemanticAnomalyWatchdog::start(config, slow_embed, None);
        let handle = watchdog.handle();

        // Flood the queue with more items than capacity
        let mut shed_count = 0u32;
        let flood_count = queue_cap * 10;
        for i in 0..flood_count {
            let data: Vec<u8> = (0..32).map(|j| ((i + j) % 256) as u8).collect();
            if !handle.observe_segment(1, &data) {
                shed_count += 1;
            }
        }

        prop_assert!(shed_count > 0, "should shed when flooding a small queue");

        watchdog.shutdown();
    }
}

// ── Config embedded type defaults ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    // 15. ConformalAnomalyConfig default has alpha in (0, 1)
    #[test]
    fn conformal_default_alpha_valid(_seed in 0..10u32) {
        let config = ConformalAnomalyConfig::default();
        prop_assert!(config.alpha > 0.0 && config.alpha < 1.0);
        prop_assert!(config.calibration_window > 0);
        prop_assert!(config.min_calibration > 0);
    }

    // 16. EntropyGateConfig default has non-negative entropy threshold
    #[test]
    fn entropy_gate_default_valid(_seed in 0..10u32) {
        let config = EntropyGateConfig::default();
        prop_assert!(config.min_entropy_bits_per_byte >= 0.0);
        prop_assert!(config.min_entropy_bits_per_byte <= 8.0);
    }
}
