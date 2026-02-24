//! Property-based tests for Pane Merge Resolver (ft-og6q6.3.2).
//!
//! Verifies invariants of PaneMergeResolver, MergeEvent, MergeConfig,
//! ClockAnomalyAnnotation, and merge determinism.

use frankenterm_core::event_id::{RecorderMergeKey, StreamKind};
use frankenterm_core::replay_merge::{
    make_merge_event, ClockAnomalyAnnotation, MergeConfig, MergeEvent, MergeEventPayload,
    MergeStats, PaneMergeResolver,
};
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn arb_stream_kind() -> impl Strategy<Value = StreamKind> {
    prop_oneof![
        Just(StreamKind::Lifecycle),
        Just(StreamKind::Control),
        Just(StreamKind::Ingress),
        Just(StreamKind::Egress),
    ]
}

/// Generate a single merge event with given constraints.
fn arb_merge_event(
    ts_range: std::ops::Range<u64>,
    pane_id: u64,
) -> impl Strategy<Value = MergeEvent> {
    (ts_range, arb_stream_kind(), 0..1000_u64, "[a-z0-9]{4,12}")
        .prop_map(move |(ts, sk, seq, eid)| {
            make_merge_event(ts, pane_id, sk, seq, &eid, "test", false)
        })
}

/// Generate a sorted stream of events for a single pane.
fn arb_sorted_pane_stream(
    pane_id: u64,
    max_events: usize,
) -> impl Strategy<Value = Vec<MergeEvent>> {
    prop::collection::vec(arb_merge_event(0..100_000, pane_id), 0..max_events).prop_map(
        |mut events| {
            events.sort_by(|a, b| a.merge_key.cmp(&b.merge_key));
            events
        },
    )
}

fn arb_config() -> impl Strategy<Value = MergeConfig> {
    (0..10_000_u64, any::<bool>()).prop_map(|(thresh, gaps)| MergeConfig {
        future_skew_threshold_ms: thresh,
        include_gap_markers: gaps,
    })
}

// ── Merge Invariant Properties ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // M-1: Output is always sorted by RecorderMergeKey
    #[test]
    fn merge_output_sorted(
        s1 in arb_sorted_pane_stream(1, 20),
        s2 in arb_sorted_pane_stream(2, 20),
        s3 in arb_sorted_pane_stream(3, 20),
    ) {
        let mut resolver = PaneMergeResolver::with_defaults();
        if !s1.is_empty() { resolver.add_pane_stream(1, s1); }
        if !s2.is_empty() { resolver.add_pane_stream(2, s2); }
        if !s3.is_empty() { resolver.add_pane_stream(3, s3); }
        let merged = resolver.merge();
        for i in 1..merged.len() {
            prop_assert!(
                merged[i].merge_key >= merged[i - 1].merge_key,
                "Output index {} not sorted: {:?} < {:?}",
                i, merged[i].merge_key, merged[i - 1].merge_key
            );
        }
    }

    // M-2: Total merged events equals sum of input events
    #[test]
    fn merge_preserves_count(
        s1 in arb_sorted_pane_stream(1, 15),
        s2 in arb_sorted_pane_stream(2, 15),
    ) {
        let expected = s1.len() + s2.len();
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, s1);
        resolver.add_pane_stream(2, s2);
        let merged = resolver.merge();
        prop_assert_eq!(merged.len(), expected);
    }

    // M-3: Source pane IDs in output are subset of input pane IDs
    #[test]
    fn merge_pane_ids_preserved(
        s1 in arb_sorted_pane_stream(1, 10),
        s2 in arb_sorted_pane_stream(2, 10),
        s3 in arb_sorted_pane_stream(3, 10),
    ) {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, s1);
        resolver.add_pane_stream(2, s2);
        resolver.add_pane_stream(3, s3);
        let merged = resolver.merge();
        for event in merged {
            let pid = event.source_pane_id;
            prop_assert!(
                pid == 1 || pid == 2 || pid == 3,
                "Unexpected pane_id: {}", pid
            );
        }
    }

    // M-4: Merge is deterministic (insertion order independent)
    #[test]
    fn merge_deterministic(
        s1 in arb_sorted_pane_stream(1, 15),
        s2 in arb_sorted_pane_stream(2, 15),
        s3 in arb_sorted_pane_stream(3, 15),
    ) {
        // Order A: 1, 2, 3
        let mut ra = PaneMergeResolver::with_defaults();
        ra.add_pane_stream(1, s1.clone());
        ra.add_pane_stream(2, s2.clone());
        ra.add_pane_stream(3, s3.clone());
        let ma: Vec<String> = ra.merge().iter().map(|e| e.merge_key.event_id.clone()).collect();

        // Order B: 3, 1, 2
        let mut rb = PaneMergeResolver::with_defaults();
        rb.add_pane_stream(3, s3);
        rb.add_pane_stream(1, s1);
        rb.add_pane_stream(2, s2);
        let mb: Vec<String> = rb.merge().iter().map(|e| e.merge_key.event_id.clone()).collect();

        prop_assert_eq!(ma, mb, "Merge must be deterministic regardless of insertion order");
    }

    // M-5: Empty input produces empty output
    #[test]
    fn merge_empty_panes(n in 0..5_usize) {
        let mut resolver = PaneMergeResolver::with_defaults();
        for i in 0..n {
            resolver.add_pane_stream(i as u64, vec![]);
        }
        let merged = resolver.merge();
        prop_assert_eq!(merged.len(), 0);
    }

    // M-6: Single pane preserves input order
    #[test]
    fn single_pane_order_preserved(
        events in arb_sorted_pane_stream(1, 30),
    ) {
        let ids: Vec<String> = events.iter().map(|e| e.merge_key.event_id.clone()).collect();
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, events);
        let merged = resolver.merge();
        let merged_ids: Vec<String> = merged.iter().map(|e| e.merge_key.event_id.clone()).collect();
        prop_assert_eq!(ids, merged_ids);
    }
}

// ── Gap Marker Properties ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // GM-1: With include_gap_markers=false, no gap markers in output
    #[test]
    fn gap_markers_excluded(
        n_events in 1..20_usize,
        n_gaps in 0..5_usize,
    ) {
        let config = MergeConfig {
            include_gap_markers: false,
            ..Default::default()
        };
        let mut resolver = PaneMergeResolver::new(config);

        let mut events = Vec::new();
        let mut ts = 100_u64;
        for i in 0..n_events {
            events.push(make_merge_event(ts, 1, StreamKind::Ingress, i as u64, &format!("e{i}"), "test", false));
            ts += 100;
        }
        for i in 0..n_gaps {
            events.push(make_merge_event(ts, 1, StreamKind::Control, (n_events + i) as u64, &format!("g{i}"), "gap", true));
            ts += 100;
        }
        events.sort_by(|a, b| a.merge_key.cmp(&b.merge_key));

        resolver.add_pane_stream(1, events);
        let merged = resolver.merge();
        prop_assert!(
            !merged.iter().any(|e| e.is_gap_marker),
            "Gap markers should be excluded"
        );
        prop_assert_eq!(merged.len(), n_events);
    }

    // GM-2: With include_gap_markers=true, all events present
    #[test]
    fn gap_markers_included(
        n_events in 1..20_usize,
        n_gaps in 0..5_usize,
    ) {
        let mut resolver = PaneMergeResolver::with_defaults();

        let mut events = Vec::new();
        let mut ts = 100_u64;
        for i in 0..n_events {
            events.push(make_merge_event(ts, 1, StreamKind::Ingress, i as u64, &format!("e{i}"), "test", false));
            ts += 100;
        }
        for i in 0..n_gaps {
            events.push(make_merge_event(ts, 1, StreamKind::Control, (n_events + i) as u64, &format!("g{i}"), "gap", true));
            ts += 100;
        }
        events.sort_by(|a, b| a.merge_key.cmp(&b.merge_key));

        resolver.add_pane_stream(1, events);
        let merged = resolver.merge();
        prop_assert_eq!(merged.len(), n_events + n_gaps);
    }
}

// ── Clock Anomaly Properties ───────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // CA-1: Forward monotonic timestamps produce no anomalies (threshold 0)
    #[test]
    fn no_anomaly_on_monotonic(n in 1..30_usize) {
        let mut resolver = PaneMergeResolver::with_defaults();
        let events: Vec<MergeEvent> = (0..n)
            .map(|i| make_merge_event(
                (i as u64) * 100,
                1,
                StreamKind::Ingress,
                i as u64,
                &format!("e{i}"),
                "test",
                false,
            ))
            .collect();
        resolver.add_pane_stream(1, events);
        let merged = resolver.merge();
        for event in merged {
            prop_assert!(
                event.clock_anomaly.is_none(),
                "Monotonic timestamps should not trigger anomaly"
            );
        }
    }

    // CA-2: Large forward jump triggers anomaly when threshold set
    #[test]
    fn future_skew_detected(
        threshold in 100..1000_u64,
        jump_factor in 2..10_u64,
    ) {
        let config = MergeConfig {
            future_skew_threshold_ms: threshold,
            include_gap_markers: true,
        };
        let mut resolver = PaneMergeResolver::new(config);
        let jump = threshold * jump_factor;
        resolver.add_pane_stream(
            1,
            vec![
                make_merge_event(100, 1, StreamKind::Ingress, 0, "a", "test", false),
                make_merge_event(100 + jump, 1, StreamKind::Ingress, 1, "b", "test", false),
            ],
        );
        let merged = resolver.merge();
        prop_assert_eq!(merged.len(), 2);
        prop_assert!(merged[1].clock_anomaly.is_some(), "Forward skew should be detected");
    }

    // CA-3: Anomaly count in stats matches actual annotations
    #[test]
    fn stats_anomaly_count_matches(
        threshold in 100..500_u64,
        timestamps in prop::collection::vec(0..50_000_u64, 2..20),
    ) {
        let config = MergeConfig {
            future_skew_threshold_ms: threshold,
            include_gap_markers: true,
        };
        let mut resolver = PaneMergeResolver::new(config);
        let mut events: Vec<MergeEvent> = timestamps.iter().enumerate()
            .map(|(i, &ts)| make_merge_event(ts, 1, StreamKind::Ingress, i as u64, &format!("e{i}"), "test", false))
            .collect();
        events.sort_by(|a, b| a.merge_key.cmp(&b.merge_key));
        resolver.add_pane_stream(1, events);
        let merged = resolver.merge();
        let actual_anomalies = merged.iter().filter(|e| e.clock_anomaly.is_some()).count();
        let stats = resolver.stats();
        prop_assert_eq!(stats.anomaly_count, actual_anomalies);
    }
}

// ── Serde Properties ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // S-1: MergeEvent serde roundtrip preserves fields
    #[test]
    fn merge_event_serde_roundtrip(
        ts in 0..100_000_u64,
        pane_id in 1..100_u64,
        sk in arb_stream_kind(),
        seq in 0..1000_u64,
        eid in "[a-z0-9]{4,12}",
    ) {
        let event = make_merge_event(ts, pane_id, sk, seq, &eid, "test", false);
        let json = serde_json::to_string(&event).unwrap();
        let back: MergeEvent = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(event.merge_key.recorded_at_ms, back.merge_key.recorded_at_ms);
        prop_assert_eq!(event.merge_key.pane_id, back.merge_key.pane_id);
        prop_assert_eq!(event.merge_key.stream_kind, back.merge_key.stream_kind);
        prop_assert_eq!(event.merge_key.sequence, back.merge_key.sequence);
        prop_assert_eq!(event.merge_key.event_id, back.merge_key.event_id);
        prop_assert_eq!(event.source_pane_id, back.source_pane_id);
        prop_assert_eq!(event.is_gap_marker, back.is_gap_marker);
    }

    // S-2: MergeConfig serde roundtrip
    #[test]
    fn merge_config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: MergeConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.future_skew_threshold_ms, back.future_skew_threshold_ms);
        prop_assert_eq!(config.include_gap_markers, back.include_gap_markers);
    }

    // S-3: MergeStats serde roundtrip
    #[test]
    fn merge_stats_serde_roundtrip(
        total in 0..1000_usize,
        panes in 0..100_usize,
        anomalies in 0..50_usize,
        gaps in 0..50_usize,
    ) {
        let stats = MergeStats {
            total_events: total,
            pane_count: panes,
            anomaly_count: anomalies,
            gap_marker_count: gaps,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: MergeStats = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(stats.total_events, back.total_events);
        prop_assert_eq!(stats.pane_count, back.pane_count);
        prop_assert_eq!(stats.anomaly_count, back.anomaly_count);
        prop_assert_eq!(stats.gap_marker_count, back.gap_marker_count);
    }

    // S-4: ClockAnomalyAnnotation serde roundtrip
    #[test]
    fn clock_annotation_serde_roundtrip(
        is_anomaly in any::<bool>(),
        reason in prop::option::of("[a-z ]{5,30}"),
    ) {
        let ann = ClockAnomalyAnnotation { is_anomaly, reason: reason.clone() };
        let json = serde_json::to_string(&ann).unwrap();
        let back: ClockAnomalyAnnotation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ann.is_anomaly, back.is_anomaly);
        prop_assert_eq!(ann.reason, back.reason);
    }
}

// ── Multi-Pane Scaling Properties ──────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // MS-1: Many panes merge correctly
    #[test]
    fn multi_pane_scaling(n_panes in 2..20_usize, events_per in 1..10_usize) {
        let mut resolver = PaneMergeResolver::with_defaults();
        let mut total = 0;
        for pane in 0..n_panes {
            let events: Vec<MergeEvent> = (0..events_per)
                .map(|i| make_merge_event(
                    (pane as u64) * 1000 + (i as u64) * 10,
                    pane as u64,
                    StreamKind::Ingress,
                    i as u64,
                    &format!("p{pane}_e{i}"),
                    "test",
                    false,
                ))
                .collect();
            total += events.len();
            resolver.add_pane_stream(pane as u64, events);
        }
        let merged = resolver.merge();
        prop_assert_eq!(merged.len(), total);
        // Verify sorted
        for i in 1..merged.len() {
            prop_assert!(merged[i].merge_key >= merged[i - 1].merge_key);
        }
    }

    // MS-2: Replacing a pane stream resets merge
    #[test]
    fn replace_resets_merge(
        s1 in arb_sorted_pane_stream(1, 10),
        s2 in arb_sorted_pane_stream(1, 10),
    ) {
        let mut resolver = PaneMergeResolver::with_defaults();
        resolver.add_pane_stream(1, s1);
        resolver.add_pane_stream(1, s2.clone());
        prop_assert_eq!(resolver.total_events(), s2.len());
    }

    // MS-3: Stats gap_marker_count matches actual gap events
    #[test]
    fn stats_gap_count_matches(
        n_events in 1..15_usize,
        n_gaps in 0..5_usize,
    ) {
        let mut resolver = PaneMergeResolver::with_defaults();
        let mut events = Vec::new();
        let mut ts = 100_u64;
        for i in 0..n_events {
            events.push(make_merge_event(ts, 1, StreamKind::Ingress, i as u64, &format!("e{i}"), "test", false));
            ts += 100;
        }
        for i in 0..n_gaps {
            events.push(make_merge_event(ts, 1, StreamKind::Control, (n_events + i) as u64, &format!("g{i}"), "gap", true));
            ts += 100;
        }
        events.sort_by(|a, b| a.merge_key.cmp(&b.merge_key));
        resolver.add_pane_stream(1, events);
        resolver.merge();
        let stats = resolver.stats();
        prop_assert_eq!(stats.gap_marker_count, n_gaps);
    }
}
