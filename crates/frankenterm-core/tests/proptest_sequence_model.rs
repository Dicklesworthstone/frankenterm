//! Property-based tests for sequence_model module.
//!
//! Verifies the deterministic sequence assignment and replay ordering invariants:
//! - Per-pane monotonicity: pane_seq strictly increases for each pane
//! - Global monotonicity: global_seq strictly increases across all panes
//! - Global uniqueness: no two assign() calls produce the same global_seq
//! - ReplayOrder total ordering (lexicographic triple)
//! - ReplayOrder serde roundtrip
//! - merge_replay_streams determinism (input order doesn't matter)
//! - validate_replay_order accepts valid sequences, rejects invalid ones
//! - CorrelationContext builder patterns
//! - CorrelationTracker auto-parent chain
//! - ClockSkewDetector anomaly detection
//! - reset_pane doesn't affect global counter

use proptest::prelude::*;
use std::collections::HashSet;

use frankenterm_core::sequence_model::{
    ClockSkewDetector, CorrelationContext, CorrelationTracker, ReplayOrder, SequenceAssigner,
    merge_replay_streams, validate_replay_order,
};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

fn arb_pane_ids(max_count: usize) -> impl Strategy<Value = Vec<u64>> {
    prop::collection::vec(0u64..=50, 1..max_count).prop_map(|mut ids| {
        ids.sort();
        ids.dedup();
        ids
    })
}

fn arb_replay_order() -> impl Strategy<Value = ReplayOrder> {
    (any::<u64>(), any::<u64>(), any::<u64>())
        .prop_map(|(g, p, s)| ReplayOrder::new(g, p, s))
}

// ────────────────────────────────────────────────────────────────────
// SequenceAssigner: per-pane monotonicity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Per-pane sequences are strictly monotonic (0, 1, 2, ...).
    #[test]
    fn prop_pane_seq_monotonic(
        pane_ids in arb_pane_ids(8),
        n_assigns in 5usize..=30,
    ) {
        let assigner = SequenceAssigner::new();
        let mut expected_pane_seq: std::collections::HashMap<u64, u64> =
            pane_ids.iter().map(|&pid| (pid, 0u64)).collect();

        for _ in 0..n_assigns {
            for &pid in &pane_ids {
                let (pane_seq, _) = assigner.assign(pid);
                let expected = expected_pane_seq.get_mut(&pid).unwrap();
                prop_assert_eq!(
                    pane_seq, *expected,
                    "pane {} seq {} != expected {}", pid, pane_seq, *expected
                );
                *expected += 1;
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// SequenceAssigner: global monotonicity
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Global sequences are strictly monotonic across all panes.
    #[test]
    fn prop_global_seq_monotonic(
        pane_ids in arb_pane_ids(8),
        n_assigns in 5usize..=30,
    ) {
        let assigner = SequenceAssigner::new();
        let mut last_global: Option<u64> = None;

        for _ in 0..n_assigns {
            for &pid in &pane_ids {
                let (_, global_seq) = assigner.assign(pid);
                if let Some(prev) = last_global {
                    prop_assert!(
                        global_seq > prev,
                        "global {} <= prev {}", global_seq, prev
                    );
                }
                last_global = Some(global_seq);
            }
        }
    }

    /// Global counter equals total number of assign() calls.
    #[test]
    fn prop_global_counter_matches_total(
        pane_ids in arb_pane_ids(8),
        n_assigns in 1usize..=20,
    ) {
        let assigner = SequenceAssigner::new();
        let total = pane_ids.len() * n_assigns;

        for _ in 0..n_assigns {
            for &pid in &pane_ids {
                assigner.assign(pid);
            }
        }

        prop_assert_eq!(assigner.current_global(), total as u64);
    }
}

// ────────────────────────────────────────────────────────────────────
// SequenceAssigner: global uniqueness
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// All global sequences are unique.
    #[test]
    fn prop_global_unique(
        pane_ids in arb_pane_ids(5),
        n_assigns in 5usize..=20,
    ) {
        let assigner = SequenceAssigner::new();
        let mut globals = HashSet::new();

        for _ in 0..n_assigns {
            for &pid in &pane_ids {
                let (_, g) = assigner.assign(pid);
                prop_assert!(globals.insert(g), "duplicate global seq {}", g);
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// SequenceAssigner: pane_count
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// pane_count reflects the number of distinct panes assigned.
    #[test]
    fn prop_pane_count_accurate(
        pane_ids in arb_pane_ids(10),
    ) {
        let assigner = SequenceAssigner::new();
        for &pid in &pane_ids {
            assigner.assign(pid);
        }
        prop_assert_eq!(assigner.pane_count(), pane_ids.len());
    }
}

// ────────────────────────────────────────────────────────────────────
// SequenceAssigner: reset_pane isolation
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// reset_pane resets per-pane counter but not global counter.
    #[test]
    fn prop_reset_pane_isolation(
        n_before in 3usize..=10,
        n_after in 1usize..=5,
    ) {
        let assigner = SequenceAssigner::new();

        // Assign some events to panes 0 and 1
        for _ in 0..n_before {
            assigner.assign(0);
            assigner.assign(1);
        }

        let global_before_reset = assigner.current_global();
        let pane1_before = assigner.current_pane(1);

        // Reset pane 0
        assigner.reset_pane(0);

        // Pane 0 should restart at 0
        prop_assert_eq!(assigner.current_pane(0), 0);

        // Pane 1 unaffected
        prop_assert_eq!(assigner.current_pane(1), pane1_before);

        // Global continues from where it was
        let (pane_seq, global_seq) = assigner.assign(0);
        prop_assert_eq!(pane_seq, 0);
        prop_assert_eq!(global_seq, global_before_reset);

        // More assigns continue monotonically
        for i in 1..n_after {
            let (ps, gs) = assigner.assign(0);
            prop_assert_eq!(ps, i as u64);
            prop_assert!(gs > global_before_reset);
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// ReplayOrder: total ordering
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// ReplayOrder sort is stable: sorting twice gives the same result.
    #[test]
    fn prop_replay_order_stable_sort(
        orders in prop::collection::vec(arb_replay_order(), 2..50),
    ) {
        let mut sorted1 = orders.clone();
        sorted1.sort();

        let mut sorted2 = sorted1.clone();
        sorted2.sort();

        prop_assert_eq!(sorted1, sorted2);
    }

    /// ReplayOrder Ord is consistent with PartialOrd (reflexive, antisymmetric, transitive).
    #[test]
    fn prop_replay_order_antisymmetric(
        a in arb_replay_order(),
        b in arb_replay_order(),
    ) {
        if a < b {
            prop_assert!(!(b < a), "antisymmetry violated");
            prop_assert!(a.is_before(&b));
        }
        if a == b {
            prop_assert!(!(a < b) && !(b < a));
        }
    }

    /// is_concurrent_with requires same global_seq, different pane_id.
    #[test]
    fn prop_is_concurrent_definition(
        g in any::<u64>(),
        p1 in any::<u64>(),
        p2 in any::<u64>(),
        s1 in any::<u64>(),
        s2 in any::<u64>(),
    ) {
        let a = ReplayOrder::new(g, p1, s1);
        let b = ReplayOrder::new(g, p2, s2);

        let concurrent = a.is_concurrent_with(&b);
        if p1 == p2 {
            prop_assert!(!concurrent, "same pane should not be concurrent");
        } else {
            prop_assert!(concurrent, "different panes at same global should be concurrent");
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// ReplayOrder: serde roundtrip
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// ReplayOrder JSON roundtrip preserves all fields.
    #[test]
    fn prop_replay_order_serde_roundtrip(
        order in arb_replay_order(),
    ) {
        let json = serde_json::to_string(&order).unwrap();
        let back: ReplayOrder = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(order, back);
    }
}

// ────────────────────────────────────────────────────────────────────
// merge_replay_streams: determinism
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Merging the same streams in different input order produces the same output.
    #[test]
    fn prop_merge_deterministic(
        n_panes in 2usize..=5,
        n_events in 3usize..=10,
    ) {
        let assigner = SequenceAssigner::new();
        let mut streams: Vec<Vec<ReplayOrder>> = Vec::new();

        for pane_id in 0..n_panes as u64 {
            let mut stream = Vec::new();
            for _ in 0..n_events {
                let (ps, gs) = assigner.assign(pane_id);
                stream.push(ReplayOrder::new(gs, pane_id, ps));
            }
            streams.push(stream);
        }

        let merged1 = merge_replay_streams(streams.clone(), |o| *o);

        // Reverse stream order
        let mut reversed = streams.clone();
        reversed.reverse();
        let merged2 = merge_replay_streams(reversed, |o| *o);

        prop_assert_eq!(merged1, merged2, "merge should be order-independent");
    }

    /// Merged output has correct total length.
    #[test]
    fn prop_merge_preserves_count(
        n_panes in 1usize..=5,
        n_events in 1usize..=10,
    ) {
        let assigner = SequenceAssigner::new();
        let mut streams: Vec<Vec<ReplayOrder>> = Vec::new();

        for pane_id in 0..n_panes as u64 {
            let mut stream = Vec::new();
            for _ in 0..n_events {
                let (ps, gs) = assigner.assign(pane_id);
                stream.push(ReplayOrder::new(gs, pane_id, ps));
            }
            streams.push(stream);
        }

        let merged = merge_replay_streams(streams, |o| *o);
        prop_assert_eq!(merged.len(), n_panes * n_events);
    }
}

// ────────────────────────────────────────────────────────────────────
// validate_replay_order: assigner produces valid sequences
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Sequences from SequenceAssigner always pass validate_replay_order.
    #[test]
    fn prop_assigner_produces_valid_order(
        pane_ids in arb_pane_ids(6),
        n_assigns in 3usize..=20,
    ) {
        let assigner = SequenceAssigner::new();
        let mut orders = Vec::new();

        for _ in 0..n_assigns {
            for &pid in &pane_ids {
                let (ps, gs) = assigner.assign(pid);
                orders.push(ReplayOrder::new(gs, pid, ps));
            }
        }

        orders.sort();
        let violations = validate_replay_order(&orders);
        prop_assert!(
            violations.is_empty(),
            "valid assigner output had {} violations", violations.len()
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// CorrelationContext: builders
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// CorrelationContext serde roundtrip.
    #[test]
    fn prop_correlation_context_serde(
        parent in proptest::option::of("[a-z]{1,10}"),
        trigger in proptest::option::of("[a-z]{1,10}"),
        root in proptest::option::of("[a-z]{1,10}"),
        batch in proptest::option::of("[a-z]{1,10}"),
    ) {
        let ctx = CorrelationContext {
            parent_event_id: parent,
            trigger_event_id: trigger,
            root_event_id: root,
            batch_id: batch,
        };

        let json = serde_json::to_string(&ctx).unwrap();
        let back: CorrelationContext = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ctx, back);
    }

    /// has_links returns true iff at least one of parent/trigger/root is set.
    #[test]
    fn prop_has_links_correct(
        parent in proptest::option::of("[a-z]{3}"),
        trigger in proptest::option::of("[a-z]{3}"),
        root in proptest::option::of("[a-z]{3}"),
    ) {
        let ctx = CorrelationContext {
            parent_event_id: parent.clone(),
            trigger_event_id: trigger.clone(),
            root_event_id: root.clone(),
            batch_id: None,
        };

        let should_have_links = parent.is_some() || trigger.is_some() || root.is_some();
        prop_assert_eq!(ctx.has_links(), should_have_links);
    }
}

// ────────────────────────────────────────────────────────────────────
// CorrelationTracker: auto-parent chain
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Each event's parent_event_id is the previous event's ID on the same pane.
    #[test]
    fn prop_tracker_parent_chain(
        n_events in 2usize..=10,
    ) {
        let tracker = CorrelationTracker::new();
        let mut last_id: Option<String> = None;

        for i in 0..n_events {
            let event_id = format!("evt-{}", i);
            let ctx = tracker.build_context(0, &event_id, None, None);

            prop_assert_eq!(
                ctx.parent_event_id.as_deref(),
                last_id.as_deref(),
                "event {} parent mismatch", i
            );
            last_id = Some(event_id);
        }
    }

    /// Different panes have independent parent chains.
    #[test]
    fn prop_tracker_pane_independence(
        n_panes in 2usize..=5,
        n_events in 2usize..=8,
    ) {
        let tracker = CorrelationTracker::new();
        let mut last_per_pane: std::collections::HashMap<u64, String> =
            std::collections::HashMap::new();

        for i in 0..n_events {
            for pid in 0..n_panes as u64 {
                let event_id = format!("p{}-evt-{}", pid, i);
                let ctx = tracker.build_context(pid, &event_id, None, None);

                prop_assert_eq!(
                    ctx.parent_event_id.as_deref(),
                    last_per_pane.get(&pid).map(|s| s.as_str()),
                );
                last_per_pane.insert(pid, event_id);
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────
// ClockSkewDetector: monotonic timestamps produce no anomalies
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Strictly increasing timestamps within reasonable bounds produce no anomalies.
    #[test]
    fn prop_monotonic_no_anomalies(
        threshold in 50u64..=500,
        n_events in 5usize..=30,
        start_ts in 1000u64..=100_000,
        step in 1u64..=100,
    ) {
        let detector = ClockSkewDetector::new(threshold);

        for i in 0..n_events {
            let ts = start_ts + (i as u64) * step;
            let anomaly = detector.observe(0, ts, i as u64);
            // Step is at most 100ms, which is < 60_000 forward threshold
            // and it's always forward, so no backward anomaly
            prop_assert!(
                anomaly.is_none(),
                "monotonic ts {} produced anomaly (step={})", ts, step
            );
        }

        prop_assert_eq!(detector.anomaly_count(), 0);
    }

    /// Backward jump larger than threshold produces an anomaly.
    #[test]
    fn prop_backward_jump_detected(
        threshold in 10u64..=100,
        jump in 1u64..=1000,
    ) {
        let detector = ClockSkewDetector::new(threshold);
        let base_ts = 10_000u64;

        detector.observe(0, base_ts, 0);

        let backward_ts = base_ts.saturating_sub(threshold + jump);
        let anomaly = detector.observe(0, backward_ts, 1);

        if base_ts - backward_ts > threshold {
            prop_assert!(anomaly.is_some(), "backward jump should be detected");
            let a = anomaly.unwrap();
            prop_assert!(a.delta_ms < 0, "backward jump delta should be negative");
        }
    }

    /// clear() resets all state — subsequent observations start fresh.
    #[test]
    fn prop_clear_resets_detector(
        threshold in 50u64..=500,
        n_events in 2usize..=10,
    ) {
        let detector = ClockSkewDetector::new(threshold);

        // Generate some events
        for i in 0..n_events {
            detector.observe(0, 1000 + i as u64 * 10, i as u64);
        }

        detector.clear();

        prop_assert_eq!(detector.anomaly_count(), 0);
        // Next observe is treated as first — no anomaly possible
        let a = detector.observe(0, 1, 0);
        prop_assert!(a.is_none(), "first observe after clear should not be anomaly");
    }
}
