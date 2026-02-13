//! Property-based tests for telemetry pipeline invariants.
//!
//! Bead: wa-uri8
//!
//! Validates:
//! 1. Histogram count tracks all recorded values
//! 2. Histogram retained <= max_samples
//! 3. Histogram quantile bounded by [min, max] of retained samples
//! 4. Histogram min/max track all-time extremes
//! 5. Histogram mean matches sum/count for all-time values
//! 6. Histogram eviction preserves most recent values
//! 7. CircularMetricBuffer capacity enforced
//! 8. CircularMetricBuffer total_recorded tracks all pushes
//! 9. CircularMetricBuffer latest is the last pushed snapshot
//! 10. MetricRegistry counter add is cumulative
//! 11. MetricRegistry histogram registration is idempotent
//! 12. MetricRegistry unregistered histogram records are silently dropped
//! 13. TelemetryStore::aggregate_snapshots mean/peak correct
//! 14. TelemetryStore::aggregate_snapshots empty returns None

use proptest::prelude::*;

use frankenterm_core::telemetry::{
    CircularMetricBuffer, Histogram, MetricRegistry, ResourceSnapshot, TelemetryStore,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_value() -> impl Strategy<Value = f64> {
    -10_000.0_f64..10_000.0
}

fn arb_positive_values(
    count: impl Into<proptest::collection::SizeRange>,
) -> impl Strategy<Value = Vec<f64>> {
    proptest::collection::vec(0.0_f64..10_000.0, count)
}

fn arb_max_samples() -> impl Strategy<Value = usize> {
    1_usize..200
}

fn arb_snapshot(pid: u32, ts: u64) -> ResourceSnapshot {
    ResourceSnapshot {
        pid,
        rss_bytes: 0,
        virt_bytes: 0,
        fd_count: 0,
        io_read_bytes: None,
        io_write_bytes: None,
        cpu_percent: None,
        timestamp_secs: ts,
    }
}

fn arb_snapshot_with_resources(rss: u64, fd: u64, cpu: Option<f64>) -> ResourceSnapshot {
    ResourceSnapshot {
        pid: 1,
        rss_bytes: rss,
        virt_bytes: rss * 2,
        fd_count: fd,
        io_read_bytes: None,
        io_write_bytes: None,
        cpu_percent: cpu,
        timestamp_secs: 1_700_000_000,
    }
}

// =============================================================================
// Property: Histogram count tracks all recorded values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn histogram_count_tracks(
        max_samples in arb_max_samples(),
        values in proptest::collection::vec(arb_value(), 1..100),
    ) {
        let mut h = Histogram::new("test", max_samples);
        for &v in &values {
            h.record(v);
        }
        prop_assert_eq!(h.count(), values.len() as u64);
    }
}

// =============================================================================
// Property: Histogram retained <= max_samples
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn histogram_retained_bounded(
        max_samples in arb_max_samples(),
        values in proptest::collection::vec(arb_value(), 1..200),
    ) {
        let mut h = Histogram::new("test", max_samples);
        for &v in &values {
            h.record(v);
        }
        prop_assert!(h.retained() <= max_samples,
            "retained {} should be <= max_samples {}", h.retained(), max_samples);
    }
}

// =============================================================================
// Property: Histogram quantile within retained sample range
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn histogram_quantile_bounded(
        values in proptest::collection::vec(arb_value(), 1..50),
        q in 0.0_f64..=1.0,
    ) {
        let mut h = Histogram::new("test", 1000);
        for &v in &values {
            h.record(v);
        }

        let quantile = h.quantile(q).unwrap();
        let min_val = values.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_val = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

        prop_assert!(quantile >= min_val - 1e-10,
            "quantile {} should be >= min {}", quantile, min_val);
        prop_assert!(quantile <= max_val + 1e-10,
            "quantile {} should be <= max {}", quantile, max_val);
    }
}

// =============================================================================
// Property: Histogram min/max track all-time extremes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn histogram_min_max_tracks_alltime(
        max_samples in 5_usize..20,
        values in proptest::collection::vec(arb_value(), 10..100),
    ) {
        let mut h = Histogram::new("test", max_samples);
        let mut expected_min = f64::INFINITY;
        let mut expected_max = f64::NEG_INFINITY;

        for &v in &values {
            h.record(v);
            expected_min = expected_min.min(v);
            expected_max = expected_max.max(v);
        }

        let (actual_min, actual_max) = h.min_max().unwrap();
        prop_assert!((actual_min - expected_min).abs() < 1e-10,
            "min should be {}, got {}", expected_min, actual_min);
        prop_assert!((actual_max - expected_max).abs() < 1e-10,
            "max should be {}, got {}", expected_max, actual_max);
    }
}

// =============================================================================
// Property: Histogram mean = sum/count for all-time values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn histogram_mean_consistent(
        values in arb_positive_values(1..50),
    ) {
        let mut h = Histogram::new("test", 1000);
        let mut sum = 0.0_f64;
        for &v in &values {
            h.record(v);
            sum += v;
        }

        let expected_mean = sum / values.len() as f64;
        let actual_mean = h.mean().unwrap();
        prop_assert!((actual_mean - expected_mean).abs() < 1e-6,
            "mean should be {}, got {}", expected_mean, actual_mean);
    }
}

// =============================================================================
// Property: Histogram empty returns None for quantiles
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn histogram_empty_returns_none(
        max_samples in arb_max_samples(),
    ) {
        let h = Histogram::new("test", max_samples);
        prop_assert!(h.p50().is_none());
        prop_assert!(h.p95().is_none());
        prop_assert!(h.p99().is_none());
        prop_assert!(h.mean().is_none());
        prop_assert!(h.min_max().is_none());
        prop_assert_eq!(h.count(), 0);
        prop_assert_eq!(h.retained(), 0);
    }
}

// =============================================================================
// Property: CircularMetricBuffer capacity enforced
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn buffer_capacity_enforced(
        capacity in 1_usize..50,
        n_pushes in 1_usize..100,
    ) {
        let buf = CircularMetricBuffer::new(capacity);
        for i in 0..n_pushes {
            buf.push(arb_snapshot(1, i as u64));
        }

        prop_assert!(buf.len() <= capacity,
            "len {} should be <= capacity {}", buf.len(), capacity);
        prop_assert_eq!(buf.len(), n_pushes.min(capacity));
    }
}

// =============================================================================
// Property: CircularMetricBuffer total_recorded tracks all pushes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn buffer_total_recorded_tracks(
        capacity in 1_usize..50,
        n_pushes in 1_usize..100,
    ) {
        let buf = CircularMetricBuffer::new(capacity);
        for i in 0..n_pushes {
            buf.push(arb_snapshot(1, i as u64));
        }
        prop_assert_eq!(buf.total_recorded(), n_pushes as u64);
    }
}

// =============================================================================
// Property: CircularMetricBuffer latest is last pushed
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn buffer_latest_is_last_pushed(
        n in 1_usize..50,
    ) {
        let buf = CircularMetricBuffer::new(10);
        for i in 0..n {
            buf.push(arb_snapshot(i as u32, i as u64 * 100));
        }

        let latest = buf.latest().unwrap();
        prop_assert_eq!(latest.pid, (n - 1) as u32);
        prop_assert_eq!(latest.timestamp_secs, (n - 1) as u64 * 100);
    }
}

// =============================================================================
// Property: CircularMetricBuffer empty initially
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn buffer_empty_initially(
        capacity in 1_usize..100,
    ) {
        let buf = CircularMetricBuffer::new(capacity);
        prop_assert!(buf.is_empty());
        prop_assert_eq!(buf.len(), 0);
        prop_assert_eq!(buf.total_recorded(), 0);
        prop_assert!(buf.latest().is_none());
        prop_assert_eq!(buf.capacity(), capacity);
    }
}

// =============================================================================
// Property: MetricRegistry counter cumulative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn registry_counter_cumulative(
        increments in proptest::collection::vec(1_u64..100, 1..20),
    ) {
        let reg = MetricRegistry::new();
        let mut expected_total: u64 = 0;

        for &delta in &increments {
            reg.add_counter("test", delta);
            expected_total += delta;
        }

        prop_assert_eq!(reg.counter_value("test"), expected_total);
    }
}

// =============================================================================
// Property: MetricRegistry histogram idempotent registration
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn registry_histogram_idempotent(
        n_registers in 1_usize..10,
        values in proptest::collection::vec(arb_value(), 1..20),
    ) {
        let reg = MetricRegistry::new();

        // Register once, record some values.
        reg.register_histogram("test", 1000);
        for &v in &values {
            reg.record_histogram("test", v);
        }

        // Register again — should not reset data.
        for _ in 0..n_registers {
            reg.register_histogram("test", 500);
        }

        let summaries = reg.histogram_summaries();
        prop_assert_eq!(summaries.len(), 1);
        prop_assert_eq!(summaries[0].count, values.len() as u64,
            "histogram data should be preserved after re-registration");
    }
}

// =============================================================================
// Property: MetricRegistry unregistered histogram silently drops
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn registry_unregistered_drops(
        n in 1_usize..20,
    ) {
        let reg = MetricRegistry::new();
        // Record without registering — should not panic.
        for i in 0..n {
            reg.record_histogram("nonexistent", i as f64);
        }
        prop_assert_eq!(reg.histogram_count(), 0);
    }
}

// =============================================================================
// Property: MetricRegistry counter values snapshot correct
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn registry_counter_snapshot(
        n_counters in 1_usize..10,
        value in 1_u64..100,
    ) {
        let reg = MetricRegistry::new();
        for i in 0..n_counters {
            let name = format!("counter_{}", i);
            reg.add_counter(&name, value);
        }

        let vals = reg.counter_values();
        prop_assert_eq!(vals.len(), n_counters);
        for i in 0..n_counters {
            let name = format!("counter_{}", i);
            prop_assert_eq!(*vals.get(&name).unwrap_or(&0), value,
                "counter {} should have value {}", name, value);
        }
    }
}

// =============================================================================
// Property: aggregate_snapshots mean/peak correct
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn aggregate_mean_and_peak(
        rss_values in proptest::collection::vec(1_u64..1_000_000, 1..20),
        fd_values in proptest::collection::vec(1_u64..10_000, 1..20),
    ) {
        let len = rss_values.len().min(fd_values.len());
        let snapshots: Vec<ResourceSnapshot> = (0..len)
            .map(|i| arb_snapshot_with_resources(rss_values[i], fd_values[i], None))
            .collect();

        let agg = TelemetryStore::aggregate_snapshots(1000, &snapshots).unwrap();

        let expected_mean_rss: u64 = rss_values[..len].iter().sum::<u64>() / len as u64;
        let expected_peak_rss = *rss_values[..len].iter().max().unwrap();
        let expected_mean_fd: u64 = fd_values[..len].iter().sum::<u64>() / len as u64;
        let expected_peak_fd = *fd_values[..len].iter().max().unwrap();

        prop_assert_eq!(agg.mean_rss_bytes, expected_mean_rss);
        prop_assert_eq!(agg.peak_rss_bytes, expected_peak_rss);
        prop_assert_eq!(agg.mean_fd_count, expected_mean_fd);
        prop_assert_eq!(agg.peak_fd_count, expected_peak_fd);
        prop_assert_eq!(agg.sample_count, len as u32);
    }
}

// =============================================================================
// Property: aggregate_snapshots empty returns None
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn aggregate_empty_returns_none(
        hour_ts in 0_u64..2_000_000_000,
    ) {
        prop_assert!(TelemetryStore::aggregate_snapshots(hour_ts, &[]).is_none());
    }
}

// =============================================================================
// Property: aggregate_snapshots cpu mean correct
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn aggregate_cpu_mean(
        cpu_values in proptest::collection::vec(0.0_f64..100.0, 2..20),
    ) {
        let snapshots: Vec<ResourceSnapshot> = cpu_values
            .iter()
            .map(|&cpu| arb_snapshot_with_resources(1024, 10, Some(cpu)))
            .collect();

        let agg = TelemetryStore::aggregate_snapshots(1000, &snapshots).unwrap();
        let expected_mean_cpu = cpu_values.iter().sum::<f64>() / cpu_values.len() as f64;

        let actual = agg.mean_cpu_percent.unwrap();
        prop_assert!((actual - expected_mean_cpu).abs() < 1e-6,
            "mean CPU should be {}, got {}", expected_mean_cpu, actual);
    }
}

// =============================================================================
// Property: Histogram summary consistent with histogram state
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn histogram_summary_consistent(
        max_samples in 10_usize..100,
        values in proptest::collection::vec(arb_value(), 1..50),
    ) {
        let mut h = Histogram::new("test_hist", max_samples);
        for &v in &values {
            h.record(v);
        }

        let s = h.summary();
        prop_assert_eq!(s.name, "test_hist");
        prop_assert_eq!(s.count, values.len() as u64);
        prop_assert_eq!(s.retained, h.retained() as u64);
        prop_assert!((s.mean.unwrap() - h.mean().unwrap()).abs() < 1e-10);
    }
}
