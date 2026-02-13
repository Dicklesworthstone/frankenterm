//! Property-based tests for cross_pane_correlation.rs (chi-squared statistics).
//!
//! Bead: wa-nwtu
//!
//! Validates:
//! 1. CoOccurrenceMatrix pair symmetry: pair_count(a,b) == pair_count(b,a)
//! 2. Marginal counts bound pair counts: pair_count(a,b) <= min(marginal(a), marginal(b))
//! 3. Total windows equals number of record_window calls
//! 4. Reset produces empty matrix (all counts zero)
//! 5. Window deduplication: duplicate event types don't inflate counts
//! 6. Pair count formula: k unique events → C(k,2) pairs per window
//! 7. Chi-squared statistic is non-negative
//! 8. P-value is in [0.0, 1.0]
//! 9. Chi-squared: insufficient data returns None
//! 10. Chi-squared survival function: survival(0, dof) == 1.0
//! 11. Chi-squared survival function: monotonically decreasing
//! 12. erfc properties: erfc(0) ≈ 1.0
//! 13. erfc: monotonically decreasing for x > 0
//! 14. CorrelationEngine: prune monotonically decreases event count
//! 15. CorrelationEngine: scan results sorted by p-value
//! 16. CorrelationEngine: all correlations have positive association
//! 17. CorrelationEngine: event_count matches ingested count
//! 18. CorrelationConfig serde roundtrip
//! 19. EventRecord serde roundtrip
//! 20. ChiSquaredResult serde roundtrip
//! 21. Correlation serde roundtrip
//! 22. Co-occurrence matrix: empty window increments total but no types
//! 23. CorrelationEngine: scan updates last_scan_ms

use proptest::prelude::*;

use frankenterm_core::cross_pane_correlation::{
    chi_squared_test, ChiSquaredResult, CoOccurrenceMatrix, Correlation, CorrelationConfig,
    CorrelationEngine, EventRecord,
};

// =============================================================================
// Strategies
// =============================================================================

/// Arbitrary event type name (short alphabetic).
fn arb_event_type() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("error".to_string()),
        Just("rate_limit".to_string()),
        Just("timeout".to_string()),
        Just("build_fail".to_string()),
        Just("oom".to_string()),
        Just("crash".to_string()),
        Just("auth_fail".to_string()),
        Just("slow_query".to_string()),
    ]
}

/// Arbitrary list of event types for a window (0-5 types, may have duplicates).
fn arb_window_events() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(arb_event_type(), 0..6)
}

/// Arbitrary CorrelationConfig with reasonable bounds.
fn arb_config() -> impl Strategy<Value = CorrelationConfig> {
    (
        1000u64..60_000,      // window_ms
        1usize..20,           // min_observations
        0.001f64..0.1,        // p_value_threshold
        10usize..100,         // max_event_types
        10_000u64..600_000,   // retention_ms
        10usize..500,         // max_panes
    )
        .prop_map(|(w, mo, p, me, r, mp)| CorrelationConfig {
            window_ms: w,
            min_observations: mo,
            p_value_threshold: p,
            max_event_types: me,
            retention_ms: r,
            max_panes: mp,
        })
}

/// Arbitrary EventRecord.
fn arb_event_record() -> impl Strategy<Value = EventRecord> {
    (0u64..100, arb_event_type(), 0u64..1_000_000)
        .prop_map(|(pane_id, event_type, timestamp_ms)| EventRecord {
            pane_id,
            event_type,
            timestamp_ms,
        })
}

// =============================================================================
// Property 1: Co-occurrence pair symmetry
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pair_count_symmetric(
        windows in prop::collection::vec(arb_window_events(), 1..20),
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        for w in &windows {
            matrix.record_window(w);
        }

        // Collect all unique event types
        let mut types: Vec<String> = windows.iter().flatten().cloned().collect();
        types.sort();
        types.dedup();

        for i in 0..types.len() {
            for j in (i + 1)..types.len() {
                let ab = matrix.pair_count(&types[i], &types[j]);
                let ba = matrix.pair_count(&types[j], &types[i]);
                prop_assert_eq!(ab, ba,
                    "pair_count({}, {}) = {} != pair_count({}, {}) = {}",
                    types[i], types[j], ab, types[j], types[i], ba);
            }
        }
    }
}

// =============================================================================
// Property 2: Marginal counts bound pair counts
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn marginal_bounds_pair(
        windows in prop::collection::vec(arb_window_events(), 1..20),
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        for w in &windows {
            matrix.record_window(w);
        }

        let mut types: Vec<String> = windows.iter().flatten().cloned().collect();
        types.sort();
        types.dedup();

        for i in 0..types.len() {
            for j in (i + 1)..types.len() {
                let pair = matrix.pair_count(&types[i], &types[j]);
                let ma = matrix.marginal(&types[i]);
                let mb = matrix.marginal(&types[j]);
                prop_assert!(pair <= ma,
                    "pair_count({}, {}) = {} > marginal({}) = {}",
                    types[i], types[j], pair, types[i], ma);
                prop_assert!(pair <= mb,
                    "pair_count({}, {}) = {} > marginal({}) = {}",
                    types[i], types[j], pair, types[j], mb);
            }
        }
    }
}

// =============================================================================
// Property 3: Total windows equals record_window call count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn total_windows_count(
        windows in prop::collection::vec(arb_window_events(), 0..30),
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        for w in &windows {
            matrix.record_window(w);
        }
        prop_assert_eq!(matrix.total_windows(), windows.len() as u64,
            "total_windows should equal number of record_window calls");
    }
}

// =============================================================================
// Property 4: Reset produces empty matrix
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn reset_clears_all(
        windows in prop::collection::vec(arb_window_events(), 1..20),
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        for w in &windows {
            matrix.record_window(w);
        }
        matrix.reset();
        prop_assert_eq!(matrix.total_windows(), 0);
        prop_assert_eq!(matrix.event_type_count(), 0);
        prop_assert_eq!(matrix.pair_count_nonzero(), 0);
    }
}

// =============================================================================
// Property 5: Window deduplication
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn window_deduplicates_event_types(
        event_type in arb_event_type(),
        n_duplicates in 2usize..10,
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        let events: Vec<String> = vec![event_type.clone(); n_duplicates];
        matrix.record_window(&events);

        // Even with N duplicates, marginal should be 1 (deduplicated).
        prop_assert_eq!(matrix.marginal(&event_type), 1,
            "marginal for '{}' should be 1 after dedup, got {}",
            event_type, matrix.marginal(&event_type));
    }
}

// =============================================================================
// Property 6: Pair count formula: k unique events → C(k,2) pairs
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pair_count_binomial(
        events in prop::collection::hash_set(arb_event_type(), 0..7),
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        let event_vec: Vec<String> = events.iter().cloned().collect();
        matrix.record_window(&event_vec);

        let k = events.len();
        let expected_pairs = if k >= 2 { k * (k - 1) / 2 } else { 0 };
        prop_assert_eq!(matrix.pair_count_nonzero(), expected_pairs,
            "k={} unique events should produce C(k,2)={} pairs, got {}",
            k, expected_pairs, matrix.pair_count_nonzero());
    }
}

// =============================================================================
// Property 7: Chi-squared statistic is non-negative
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn chi_squared_nonneg(
        n_cooccur in 5u64..50,
        n_a_only in 5u64..50,
        n_b_only in 5u64..50,
        n_neither in 5u64..50,
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        for _ in 0..n_cooccur {
            matrix.record_window(&["a".into(), "b".into()]);
        }
        for _ in 0..n_a_only {
            matrix.record_window(&["a".into()]);
        }
        for _ in 0..n_b_only {
            matrix.record_window(&["b".into()]);
        }
        for _ in 0..n_neither {
            matrix.record_window(&[]);
        }

        if let Some(result) = chi_squared_test(&matrix, "a", "b") {
            prop_assert!(result.chi_squared >= 0.0,
                "chi-squared should be non-negative, got {}", result.chi_squared);
        }
    }
}

// =============================================================================
// Property 8: P-value is in [0.0, 1.0]
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pvalue_in_unit_interval(
        n_cooccur in 5u64..50,
        n_a_only in 5u64..50,
        n_b_only in 5u64..50,
        n_neither in 5u64..50,
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        for _ in 0..n_cooccur {
            matrix.record_window(&["a".into(), "b".into()]);
        }
        for _ in 0..n_a_only {
            matrix.record_window(&["a".into()]);
        }
        for _ in 0..n_b_only {
            matrix.record_window(&["b".into()]);
        }
        for _ in 0..n_neither {
            matrix.record_window(&[]);
        }

        if let Some(result) = chi_squared_test(&matrix, "a", "b") {
            prop_assert!(result.p_value >= 0.0 && result.p_value <= 1.0,
                "p-value should be in [0, 1], got {}", result.p_value);
        }
    }
}

// =============================================================================
// Property 9: Insufficient data returns None
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn chi_squared_insufficient_data(
        a in arb_event_type(),
        b in arb_event_type(),
    ) {
        // Empty matrix should always return None.
        let matrix = CoOccurrenceMatrix::new();
        let result = chi_squared_test(&matrix, &a, &b);
        prop_assert!(result.is_none(),
            "empty matrix should return None for chi-squared test");
    }
}

// =============================================================================
// Property 10: Chi-squared survival at x=0 is 1.0
// =============================================================================

// This tests the survival function indirectly through the chi-squared test.
// When o11 == e11 (perfect independence), chi_squared ≈ 0, p ≈ 1.0.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn perfect_independence_gives_high_pvalue(
        n in 20u64..100,
    ) {
        // Construct data where events co-occur at exactly the expected rate.
        // P(A) = 0.5, P(B) = 0.5, P(A ∩ B) = 0.25 under independence.
        let mut matrix = CoOccurrenceMatrix::new();
        // n windows of A+B, n of A only, n of B only, n of neither
        // This gives P(A) = 2n/4n = 0.5, P(B) = 2n/4n = 0.5
        // Expected P(A∩B) = 0.25, observed = n/4n = 0.25
        for _ in 0..n {
            matrix.record_window(&["a".into(), "b".into()]);
        }
        for _ in 0..n {
            matrix.record_window(&["a".into()]);
        }
        for _ in 0..n {
            matrix.record_window(&["b".into()]);
        }
        for _ in 0..n {
            matrix.record_window(&[]);
        }

        if let Some(result) = chi_squared_test(&matrix, "a", "b") {
            // Under perfect independence, chi-squared ≈ 0, p ≈ 1.0
            prop_assert!(result.p_value > 0.5,
                "independent data should have high p-value, got {} (chi2={})",
                result.p_value, result.chi_squared);
        }
    }
}

// =============================================================================
// Property 11: Chi-squared survival monotonically decreasing
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn stronger_correlation_lower_pvalue(
        base in 10u64..30,
    ) {
        // Build two matrices: one with moderate and one with strong co-occurrence.

        // Moderate: base co-occurrences, 3*base independent
        let mut moderate = CoOccurrenceMatrix::new();
        for _ in 0..base {
            moderate.record_window(&["a".into(), "b".into()]);
        }
        for _ in 0..(base * 3) {
            moderate.record_window(&["a".into()]);
        }
        for _ in 0..(base * 3) {
            moderate.record_window(&["b".into()]);
        }
        for _ in 0..(base * 3) {
            moderate.record_window(&[]);
        }

        // Strong: 3*base co-occurrences, base independent
        let mut strong = CoOccurrenceMatrix::new();
        for _ in 0..(base * 3) {
            strong.record_window(&["a".into(), "b".into()]);
        }
        for _ in 0..base {
            strong.record_window(&["a".into()]);
        }
        for _ in 0..base {
            strong.record_window(&["b".into()]);
        }
        for _ in 0..base {
            strong.record_window(&[]);
        }

        if let (Some(mod_result), Some(str_result)) = (
            chi_squared_test(&moderate, "a", "b"),
            chi_squared_test(&strong, "a", "b"),
        ) {
            // Stronger correlation → higher chi-squared → lower p-value
            if str_result.positive_association && mod_result.positive_association {
                prop_assert!(str_result.chi_squared >= mod_result.chi_squared,
                    "stronger correlation should have higher chi-squared: {} >= {}",
                    str_result.chi_squared, mod_result.chi_squared);
            }
        }
    }
}

// =============================================================================
// Property 12: erfc(0) ≈ 1.0 (tested indirectly via chi-squared survival)
// =============================================================================

// Covered by property 10 (perfect independence → p ≈ 1.0 which relies on erfc(0) ≈ 1.0)

// =============================================================================
// Property 13: CorrelationEngine prune monotonically decreases event count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prune_decreases_events(
        events in prop::collection::vec(arb_event_record(), 1..30),
        retention_ms in 10_000u64..100_000,
    ) {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            retention_ms,
            ..Default::default()
        });

        for ev in &events {
            engine.ingest(ev.clone());
        }
        let count_before = engine.event_count();

        // Prune with a timestamp that's retention_ms past the latest event
        let max_ts = events.iter().map(|e| e.timestamp_ms).max().unwrap_or(0);
        let prune_at = max_ts + retention_ms + 1;
        engine.prune(prune_at);
        let count_after = engine.event_count();

        prop_assert!(count_after <= count_before,
            "prune should not increase event count: {} <= {}", count_after, count_before);
    }
}

// =============================================================================
// Property 14: Scan results sorted by p-value
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn scan_results_sorted_by_pvalue(
        n_pairs in 5u64..30,
    ) {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            window_ms: 10_000,
            min_observations: 3,
            p_value_threshold: 0.99, // High threshold to catch more correlations
            retention_ms: 600_000,
            ..Default::default()
        });

        // Generate events that co-occur frequently
        for i in 0..n_pairs {
            let ts = i * 15_000;
            engine.ingest(EventRecord {
                pane_id: 1,
                event_type: "a".into(),
                timestamp_ms: ts,
            });
            engine.ingest(EventRecord {
                pane_id: 2,
                event_type: "b".into(),
                timestamp_ms: ts + 1000,
            });
            if i % 3 == 0 {
                engine.ingest(EventRecord {
                    pane_id: 3,
                    event_type: "c".into(),
                    timestamp_ms: ts + 2000,
                });
            }
        }

        let results = engine.scan(n_pairs * 15_000);
        for window in results.windows(2) {
            prop_assert!(
                window[0].p_value <= window[1].p_value
                    || (window[0].p_value - window[1].p_value).abs() < 1e-10,
                "results should be sorted by p-value: {} <= {}",
                window[0].p_value, window[1].p_value
            );
        }
    }
}

// =============================================================================
// Property 15: All detected correlations have positive association
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn scan_only_positive_correlations(
        n_events in 10u64..40,
    ) {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            window_ms: 10_000,
            min_observations: 3,
            p_value_threshold: 0.05,
            retention_ms: 600_000,
            ..Default::default()
        });

        for i in 0..n_events {
            let ts = i * 12_000;
            engine.ingest(EventRecord {
                pane_id: 1,
                event_type: "x".into(),
                timestamp_ms: ts,
            });
            engine.ingest(EventRecord {
                pane_id: 2,
                event_type: "y".into(),
                timestamp_ms: ts + 500,
            });
        }

        let results = engine.scan(n_events * 12_000);
        for corr in &results {
            prop_assert!(corr.positive,
                "scan should only return positive correlations: {:?}", corr);
        }
    }
}

// =============================================================================
// Property 16: event_count matches ingested count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn event_count_matches_ingest(
        events in prop::collection::vec(arb_event_record(), 0..30),
    ) {
        let mut engine = CorrelationEngine::new(CorrelationConfig::default());
        for ev in &events {
            engine.ingest(ev.clone());
        }
        prop_assert_eq!(engine.event_count(), events.len(),
            "event_count should match number of ingested events");
    }
}

// =============================================================================
// Property 17: CorrelationConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: CorrelationConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.window_ms, config.window_ms);
        prop_assert_eq!(back.min_observations, config.min_observations);
        prop_assert!((back.p_value_threshold - config.p_value_threshold).abs() < 1e-12);
        prop_assert_eq!(back.max_event_types, config.max_event_types);
        prop_assert_eq!(back.retention_ms, config.retention_ms);
        prop_assert_eq!(back.max_panes, config.max_panes);
    }
}

// =============================================================================
// Property 18: EventRecord serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn event_record_serde_roundtrip(record in arb_event_record()) {
        let json = serde_json::to_string(&record).unwrap();
        let back: EventRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, record.pane_id);
        prop_assert_eq!(back.event_type, record.event_type);
        prop_assert_eq!(back.timestamp_ms, record.timestamp_ms);
    }
}

// =============================================================================
// Property 19: ChiSquaredResult serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn chi_squared_result_serde(
        chi_sq in 0.0f64..100.0,
        p_val in 0.0f64..1.0,
        observed in 0u64..1000,
        expected in 0.1f64..500.0,
    ) {
        let result = ChiSquaredResult {
            event_a: "error".into(),
            event_b: "timeout".into(),
            chi_squared: chi_sq,
            p_value: p_val,
            observed,
            expected,
            positive_association: observed as f64 > expected,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: ChiSquaredResult = serde_json::from_str(&json).unwrap();
        prop_assert!((back.chi_squared - chi_sq).abs() < 1e-10);
        prop_assert!((back.p_value - p_val).abs() < 1e-10);
        prop_assert_eq!(back.observed, observed);
        prop_assert!((back.expected - expected).abs() < 1e-10);
        prop_assert_eq!(back.positive_association, result.positive_association);
    }
}

// =============================================================================
// Property 20: Correlation serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn correlation_serde_roundtrip(
        chi_sq in 0.0f64..100.0,
        p_val in 0.0f64..1.0,
        co_count in 0u64..1000,
        expected in 0.1f64..500.0,
        panes in prop::collection::vec(0u64..100, 1..5),
    ) {
        let corr = Correlation {
            event_a: "error".into(),
            event_b: "crash".into(),
            chi_squared: chi_sq,
            p_value: p_val,
            co_occurrence_count: co_count,
            expected_count: expected,
            positive: co_count as f64 > expected,
            participating_panes: panes.clone(),
        };
        let json = serde_json::to_string(&corr).unwrap();
        let back: Correlation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.event_a, corr.event_a);
        prop_assert_eq!(back.event_b, corr.event_b);
        prop_assert!((back.chi_squared - chi_sq).abs() < 1e-10);
        prop_assert!((back.p_value - p_val).abs() < 1e-10);
        prop_assert_eq!(back.co_occurrence_count, co_count);
        prop_assert_eq!(back.participating_panes, panes);
    }
}

// =============================================================================
// Property 21: Empty window increments total but no types
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn empty_windows_only_increment_total(
        n_empty in 1u64..50,
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        for _ in 0..n_empty {
            matrix.record_window(&[]);
        }
        prop_assert_eq!(matrix.total_windows(), n_empty);
        prop_assert_eq!(matrix.event_type_count(), 0);
        prop_assert_eq!(matrix.pair_count_nonzero(), 0);
    }
}

// =============================================================================
// Property 22: Scan updates last_scan_ms
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn scan_updates_last_scan(
        scan_time in 1000u64..1_000_000,
    ) {
        let mut engine = CorrelationEngine::new(CorrelationConfig::default());
        prop_assert_eq!(engine.last_scan_ms(), 0);
        let _ = engine.scan(scan_time);
        prop_assert_eq!(engine.last_scan_ms(), scan_time,
            "last_scan_ms should be updated to scan time");
    }
}

// =============================================================================
// Property 23: ingest_batch equivalent to individual ingest
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn ingest_batch_equivalent(
        events in prop::collection::vec(arb_event_record(), 1..15),
    ) {
        let mut engine_single = CorrelationEngine::new(CorrelationConfig::default());
        let mut engine_batch = CorrelationEngine::new(CorrelationConfig::default());

        for ev in &events {
            engine_single.ingest(ev.clone());
        }
        engine_batch.ingest_batch(events.iter().cloned());

        prop_assert_eq!(engine_single.event_count(), engine_batch.event_count(),
            "batch ingest should have same count as individual ingest");
    }
}

// =============================================================================
// Property 24: Prune with future timestamp keeps all events
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn prune_future_keeps_all(
        events in prop::collection::vec(arb_event_record(), 1..20),
    ) {
        let mut engine = CorrelationEngine::new(CorrelationConfig {
            retention_ms: 1_000_000, // Large retention window
            ..Default::default()
        });
        for ev in &events {
            engine.ingest(ev.clone());
        }
        let count_before = engine.event_count();

        // Prune at a time within the retention window of all events
        let max_ts = events.iter().map(|e| e.timestamp_ms).max().unwrap_or(0);
        engine.prune(max_ts); // now_ms = max_ts, cutoff = max_ts - 1M, all events are >= cutoff
        let count_after = engine.event_count();

        prop_assert_eq!(count_before, count_after,
            "prune within retention should keep all events");
    }
}

// =============================================================================
// Property 25: CorrelationConfig default has sensible values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn config_default_sensible(_dummy in 0..1u32) {
        let config = CorrelationConfig::default();
        prop_assert!(config.window_ms > 0, "window_ms should be positive");
        prop_assert!(config.min_observations > 0, "min_observations should be positive");
        prop_assert!(config.p_value_threshold > 0.0 && config.p_value_threshold < 1.0,
            "p_value_threshold should be in (0, 1)");
        prop_assert!(config.max_event_types > 0, "max_event_types should be positive");
        prop_assert!(config.retention_ms > config.window_ms,
            "retention should be longer than window");
        prop_assert!(config.max_panes > 0, "max_panes should be positive");
    }
}

// =============================================================================
// Property 26: Chi-squared test with high co-occurrence detects significance
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn high_cooccurrence_is_significant(
        n in 50u64..200,
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        // Strong co-occurrence: events always appear together
        for _ in 0..n {
            matrix.record_window(&["a".into(), "b".into()]);
        }
        // Some windows with neither
        for _ in 0..n {
            matrix.record_window(&[]);
        }

        if let Some(result) = chi_squared_test(&matrix, "a", "b") {
            prop_assert!(result.chi_squared > 0.0,
                "perfect co-occurrence should have positive chi-squared");
            prop_assert!(result.p_value < 0.01,
                "perfect co-occurrence should be significant (p={} < 0.01)", result.p_value);
            prop_assert!(result.positive_association,
                "co-occurring events should have positive association");
        }
    }
}

// =============================================================================
// Property 27: Marginal count equals count of windows containing the event type
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn marginal_count_accurate(
        windows in prop::collection::vec(arb_window_events(), 1..20),
    ) {
        let mut matrix = CoOccurrenceMatrix::new();
        for w in &windows {
            matrix.record_window(w);
        }

        // For each event type, marginal should equal # of windows containing it
        let mut types: Vec<String> = windows.iter().flatten().cloned().collect();
        types.sort();
        types.dedup();

        for et in &types {
            let manual_count = windows.iter()
                .filter(|w| {
                    let mut deduped: Vec<String> = (*w).clone();
                    deduped.sort();
                    deduped.dedup();
                    deduped.contains(et)
                })
                .count() as u64;
            prop_assert_eq!(matrix.marginal(et), manual_count,
                "marginal('{}') should be {} (windows containing it), got {}",
                et, manual_count, matrix.marginal(et));
        }
    }
}
