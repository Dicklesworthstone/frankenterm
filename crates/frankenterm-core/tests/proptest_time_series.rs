//! Property-based tests for `time_series` — compact time-series storage.

use proptest::prelude::*;

use frankenterm_core::time_series::*;

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

fn arb_config() -> impl Strategy<Value = TimeSeriesConfig> {
    (1..500usize).prop_map(|max| TimeSeriesConfig { max_points: max })
}

fn arb_points(max_count: usize) -> impl Strategy<Value = Vec<(u64, f64)>> {
    proptest::collection::vec((0..100_000u64, -1000.0..1000.0f64), 0..max_count)
}

fn arb_sorted_points(max_count: usize) -> impl Strategy<Value = Vec<(u64, f64)>> {
    proptest::collection::vec((1..1000u64, -100.0..100.0f64), 0..max_count).prop_map(|deltas| {
        let mut ts = 0u64;
        deltas
            .into_iter()
            .map(|(delta, val)| {
                ts += delta;
                (ts, val)
            })
            .collect()
    })
}

fn build_ts(config: TimeSeriesConfig, points: &[(u64, f64)]) -> TimeSeries {
    let mut ts = TimeSeries::with_config(config);
    for &(t, v) in points {
        ts.push(t, v);
    }
    ts
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. len <= capacity
    #[test]
    fn len_bounded_by_capacity(config in arb_config(), points in arb_points(100)) {
        let ts = build_ts(config, &points);
        prop_assert!(ts.len() <= config.max_points);
    }

    // 2. total_inserted = number of pushes
    #[test]
    fn total_inserted_equals_push_count(config in arb_config(), points in arb_points(100)) {
        let ts = build_ts(config, &points);
        prop_assert_eq!(ts.total_inserted(), points.len() as u64);
    }

    // 3. total_evicted = total_inserted - len
    #[test]
    fn eviction_accounting(config in arb_config(), points in arb_points(100)) {
        let ts = build_ts(config, &points);
        prop_assert_eq!(ts.total_evicted(), ts.total_inserted() - ts.len() as u64);
    }

    // 4. is_empty iff len == 0
    #[test]
    fn is_empty_iff_len_zero(config in arb_config(), points in arb_points(50)) {
        let ts = build_ts(config, &points);
        prop_assert_eq!(ts.is_empty(), ts.len() == 0);
    }

    // 5. latest is the last pushed point (when not evicted)
    #[test]
    fn latest_is_last_pushed(config in arb_config(), points in arb_sorted_points(50)) {
        prop_assume!(!points.is_empty());
        let ts = build_ts(config, &points);
        let last_point = points.last().unwrap();
        let latest = ts.latest().unwrap();
        prop_assert_eq!(latest.timestamp_ms, last_point.0);
    }

    // 6. time_span >= 0
    #[test]
    fn time_span_non_negative(config in arb_config(), points in arb_sorted_points(50)) {
        let ts = build_ts(config, &points);
        // time_span uses saturating_sub so always >= 0
        let _ = ts.time_span_ms();
    }

    // 7. range returns subset of stored points
    #[test]
    fn range_is_subset(config in arb_config(), points in arb_sorted_points(50)) {
        let ts = build_ts(config, &points);
        let range = ts.range(0, u64::MAX);
        prop_assert_eq!(range.len(), ts.len());
    }

    // 8. range respects bounds
    #[test]
    fn range_respects_bounds(config in arb_config(), points in arb_sorted_points(50),
                            start in 0..50_000u64, end in 50_000..100_000u64) {
        let ts = build_ts(config, &points);
        let range = ts.range(start, end);
        for dp in &range {
            prop_assert!(dp.timestamp_ms >= start);
            prop_assert!(dp.timestamp_ms <= end);
        }
    }

    // 9. stats count matches range count
    #[test]
    fn stats_count_matches_range(config in arb_config(), points in arb_sorted_points(50)) {
        let ts = build_ts(config, &points);
        let range = ts.range(0, u64::MAX);
        let stats = ts.stats(0, u64::MAX);
        match stats {
            Some(s) => prop_assert_eq!(s.count, range.len()),
            None => prop_assert!(range.is_empty()),
        }
    }

    // 10. stats min <= mean <= max
    #[test]
    fn stats_min_mean_max_order(config in arb_config(), points in arb_sorted_points(50)) {
        prop_assume!(!points.is_empty());
        let ts = build_ts(config, &points);
        if let Some(s) = ts.stats_all() {
            prop_assert!(s.min <= s.mean, "min={} > mean={}", s.min, s.mean);
            prop_assert!(s.mean <= s.max, "mean={} > max={}", s.mean, s.max);
        }
    }

    // 11. stats sum = mean * count
    #[test]
    fn stats_sum_equals_mean_times_count(config in arb_config(), points in arb_sorted_points(50)) {
        prop_assume!(!points.is_empty());
        let ts = build_ts(config, &points);
        if let Some(s) = ts.stats_all() {
            let expected_sum = s.mean * s.count as f64;
            prop_assert!((s.sum - expected_sum).abs() < 0.01,
                "sum={}, expected={}", s.sum, expected_sum);
        }
    }

    // 12. percentile(0.0) = min
    #[test]
    fn percentile_zero_is_min(config in arb_config(), points in arb_sorted_points(50)) {
        prop_assume!(!points.is_empty());
        let ts = build_ts(config, &points);
        let p0 = ts.percentile(0.0).unwrap();
        let stats = ts.stats_all().unwrap();
        prop_assert!((p0 - stats.min).abs() < f64::EPSILON);
    }

    // 13. percentile(1.0) = max
    #[test]
    fn percentile_one_is_max(config in arb_config(), points in arb_sorted_points(50)) {
        prop_assume!(!points.is_empty());
        let ts = build_ts(config, &points);
        let p100 = ts.percentile(1.0).unwrap();
        let stats = ts.stats_all().unwrap();
        prop_assert!((p100 - stats.max).abs() < f64::EPSILON);
    }

    // 14. percentile monotonically non-decreasing
    #[test]
    fn percentile_monotonic(config in arb_config(), points in arb_sorted_points(50)) {
        prop_assume!(points.len() >= 2);
        let ts = build_ts(config, &points);
        let p25 = ts.percentile(0.25).unwrap();
        let p50 = ts.percentile(0.50).unwrap();
        let p75 = ts.percentile(0.75).unwrap();
        prop_assert!(p25 <= p50);
        prop_assert!(p50 <= p75);
    }

    // 15. clear makes empty
    #[test]
    fn clear_makes_empty(config in arb_config(), points in arb_points(50)) {
        let mut ts = build_ts(config, &points);
        ts.clear();
        prop_assert!(ts.is_empty());
        prop_assert_eq!(ts.len(), 0);
    }

    // 16. downsample produces at most target_points
    #[test]
    fn downsample_bounded(config in arb_config(), points in arb_sorted_points(100), target in 1..50usize) {
        let ts = build_ts(config, &points);
        let ds = ts.downsample(target);
        prop_assert!(ds.len() <= target);
    }

    // 17. downsample of empty is empty
    #[test]
    fn downsample_empty_is_empty(target in 1..50usize) {
        let ts = TimeSeries::new();
        let ds = ts.downsample(target);
        prop_assert!(ds.is_empty());
    }

    // 18. downsample preserves approximate time range
    #[test]
    fn downsample_preserves_range(config in arb_config(), points in arb_sorted_points(100)) {
        prop_assume!(points.len() >= 2);
        let ts = build_ts(config, &points);
        prop_assume!(ts.len() >= 2);
        let ds = ts.downsample(10);
        if ds.len() >= 2 {
            // First downsampled point should be near start
            let orig_start = ts.oldest().unwrap().timestamp_ms;
            let ds_start = ds.oldest().unwrap().timestamp_ms;
            let orig_end = ts.latest().unwrap().timestamp_ms;
            let ds_end = ds.latest().unwrap().timestamp_ms;
            // Downsampled range should be within original range
            prop_assert!(ds_start >= orig_start);
            prop_assert!(ds_end <= orig_end);
        }
    }

    // 19. merge increases length (up to capacity)
    #[test]
    fn merge_increases_or_caps(config in arb_config(),
                               p1 in arb_sorted_points(30),
                               p2 in arb_sorted_points(30)) {
        let mut ts1 = build_ts(config, &p1);
        let ts2 = build_ts(TimeSeriesConfig { max_points: 10_000 }, &p2);
        let len_before = ts1.len();
        ts1.merge(&ts2);
        prop_assert!(ts1.len() >= len_before.min(config.max_points));
        prop_assert!(ts1.len() <= config.max_points);
    }

    // 20. merge result is sorted by timestamp
    #[test]
    fn merge_sorted(config in arb_config(),
                    p1 in arb_sorted_points(30),
                    p2 in arb_sorted_points(30)) {
        let mut ts1 = build_ts(config, &p1);
        let ts2 = build_ts(TimeSeriesConfig { max_points: 10_000 }, &p2);
        ts1.merge(&ts2);
        let points = ts1.to_vec();
        for w in points.windows(2) {
            prop_assert!(w[0].timestamp_ms <= w[1].timestamp_ms);
        }
    }

    // 21. serde roundtrip preserves structure
    #[test]
    fn serde_roundtrip(config in arb_config(), points in arb_sorted_points(50)) {
        let ts = build_ts(config, &points);
        let json = serde_json::to_string(&ts).unwrap();
        let back: TimeSeries = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ts.len(), back.len());
        prop_assert_eq!(ts.capacity(), back.capacity());
        prop_assert_eq!(ts.total_inserted(), back.total_inserted());
        // Timestamps preserved exactly; values may have float precision variance
        let orig = ts.to_vec();
        let restored = back.to_vec();
        for (a, b) in orig.iter().zip(restored.iter()) {
            prop_assert_eq!(a.timestamp_ms, b.timestamp_ms);
            prop_assert!((a.value - b.value).abs() < 1e-10,
                "value mismatch: {} vs {}", a.value, b.value);
        }
    }

    // 22. config serde roundtrip
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: TimeSeriesConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config, back);
    }

    // 23. clone equality
    #[test]
    fn clone_eq(config in arb_config(), points in arb_sorted_points(50)) {
        let ts = build_ts(config, &points);
        let cloned = ts.clone();
        prop_assert_eq!(ts, cloned);
    }

    // 24. rate is non-negative
    #[test]
    fn rate_non_negative(config in arb_config(), points in arb_sorted_points(50),
                         now in 0..200_000u64, window in 1..100_000u64) {
        let ts = build_ts(config, &points);
        prop_assert!(ts.rate(now, window) >= 0.0);
    }

    // 25. to_vec length equals len
    #[test]
    fn to_vec_matches_len(config in arb_config(), points in arb_sorted_points(50)) {
        let ts = build_ts(config, &points);
        prop_assert_eq!(ts.to_vec().len(), ts.len());
    }

    // 26. iter count equals len
    #[test]
    fn iter_count_matches_len(config in arb_config(), points in arb_sorted_points(50)) {
        let ts = build_ts(config, &points);
        prop_assert_eq!(ts.iter().count(), ts.len());
    }

    // 27. capacity matches config
    #[test]
    fn capacity_matches_config(config in arb_config()) {
        let ts = TimeSeries::with_config(config);
        prop_assert_eq!(ts.capacity(), config.max_points);
    }

    // 28. DataPoint serde roundtrip (timestamps exact, values approximate)
    #[test]
    fn data_point_serde(t in 0..100_000u64, v in -1000.0..1000.0f64) {
        let dp = DataPoint::new(t, v);
        let json = serde_json::to_string(&dp).unwrap();
        let back: DataPoint = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(dp.timestamp_ms, back.timestamp_ms);
        prop_assert!((dp.value - back.value).abs() < 1e-10,
            "value mismatch: {} vs {}", dp.value, back.value);
    }

    // 29. stats empty for empty range
    #[test]
    fn stats_none_for_empty(config in arb_config()) {
        let ts = TimeSeries::with_config(config);
        prop_assert!(ts.stats(0, 1000).is_none());
        prop_assert!(ts.stats_all().is_none());
    }

    // 30. single point stats are trivial
    #[test]
    fn single_point_stats(config in arb_config(), t in 0..100_000u64, v in -100.0..100.0f64) {
        let mut ts = TimeSeries::with_config(config);
        ts.push(t, v);
        let s = ts.stats_all().unwrap();
        prop_assert_eq!(s.count, 1);
        prop_assert!((s.min - v).abs() < f64::EPSILON);
        prop_assert!((s.max - v).abs() < f64::EPSILON);
        prop_assert!((s.mean - v).abs() < f64::EPSILON);
        prop_assert!((s.sum - v).abs() < f64::EPSILON);
    }
}
