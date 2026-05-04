//! Property tests for [`shadow_experiment_harness`] (ft-1650n.12).
//!
//! Pins the harness's documented invariants over arbitrary
//! `(record_pair | abort_manually)` operation sequences.
//! Complements the substrate's unit tests at
//! `shadow_experiment_harness.rs::tests` (14 cases including
//! the linter-extended 50-pane synthetic-fleet test).
//!
//! Properties pinned here:
//!
//! 1. **Ledger size ≤ max_events** — the decisions vec never
//!    exceeds the configured `max_events` cap.
//! 2. **Cumulative overhead ≤ max_total_overhead** — the
//!    ledger's `total_overhead_us` never exceeds the budget at
//!    any observation point.
//! 3. **Per-event overhead ≤ max_overhead_us_per_event for
//!    accepted pairs** — every recorded pair's `overhead_us` is
//!    within the per-event cap.
//! 4. **Post-abort calls bump sampling_gaps** — after
//!    `is_aborted()` is true, every subsequent `record_pair`
//!    returns false AND increments `sampling_gaps`.
//! 5. **abort_reason is Some when aborted** — `is_aborted() ⇒
//!    ledger.abort_reason.is_some()`.
//! 6. **abort_manually is idempotent** — calling it twice
//!    preserves the first reason.
//! 7. **Counter monotonicity** — every counter only increases
//!    across operations.
//! 8. **divergence_counts conservation** — `match_count +
//!    minor_count + major_count == decisions.len()`.
//! 9. **record_pair return value matches written-state** —
//!    `record_pair` returns true iff the decision was
//!    appended to `decisions`.

use std::sync::Once;

use frankenterm_core::shadow_experiment_harness::{
    DivergenceKind, ExperimentBudget, ExperimentManifest, ShadowDecisionPair,
    ShadowExperimentHarness,
};
use proptest::prelude::*;

fn init_test_tracing_json() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .json()
            .with_target(true)
            .with_test_writer()
            .try_init();
    });
}

fn manifest(budget: ExperimentBudget) -> ExperimentManifest {
    ExperimentManifest {
        experiment_id: "exp-1".to_string(),
        baseline_label: "baseline".to_string(),
        candidate_label: "candidate".to_string(),
        budget,
        redaction_policy_version: "redaction-v1".to_string(),
    }
}

fn arb_divergence() -> impl Strategy<Value = DivergenceKind> {
    prop_oneof![
        Just(DivergenceKind::Match),
        Just(DivergenceKind::Minor),
        Just(DivergenceKind::Major),
    ]
}

fn arb_pair(idx: usize) -> impl Strategy<Value = ShadowDecisionPair> {
    let event_id = format!("e{idx}");
    (arb_divergence(), 0u64..=20_000).prop_map(move |(div, overhead)| ShadowDecisionPair {
        event_id: event_id.clone(),
        baseline_decision: "b".to_string(),
        candidate_decision: "c".to_string(),
        divergence: div,
        overhead_us: overhead,
        evidence_citations: vec!["pattern_hit".to_string()],
    })
}

fn arb_pair_sequence() -> impl Strategy<Value = Vec<ShadowDecisionPair>> {
    (1usize..=20).prop_flat_map(|n| (0..n).map(arb_pair).collect::<Vec<_>>())
}

/// Bounded budget generator. Caps stay small so the
/// abort/cap interactions get exercised without explosion.
fn arb_budget() -> impl Strategy<Value = ExperimentBudget> {
    (1u64..=15, 1u64..=15_000, 1u64..=120_000).prop_map(|(max_events, max_per_event, max_total)| {
        ExperimentBudget {
            max_events,
            max_overhead_us_per_event: max_per_event,
            max_total_overhead_us: max_total,
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// **Ledger size ≤ max_events**: at any observation point
    /// the decisions vec never exceeds the configured
    /// `max_events` cap.
    #[test]
    fn ledger_size_bounded_by_max_events(
        budget in arb_budget(),
        pairs in arb_pair_sequence(),
    ) {
        init_test_tracing_json();
        let mut h = ShadowExperimentHarness::new(manifest(budget));
        for p in pairs {
            h.record_pair(p);
            let len = h.ledger().decisions.len() as u64;
            prop_assert!(
                len <= budget.max_events,
                "decisions.len() {} exceeded max_events {}",
                len, budget.max_events
            );
        }
    }

    /// **Cumulative overhead ≤ max_total_overhead**: the
    /// ledger's `total_overhead_us` never exceeds the budget
    /// at any observation point.
    #[test]
    fn cumulative_overhead_bounded_by_max_total(
        budget in arb_budget(),
        pairs in arb_pair_sequence(),
    ) {
        init_test_tracing_json();
        let mut h = ShadowExperimentHarness::new(manifest(budget));
        for p in pairs {
            h.record_pair(p);
            prop_assert!(
                h.ledger().total_overhead_us <= budget.max_total_overhead_us,
                "total_overhead_us {} exceeded max_total {}",
                h.ledger().total_overhead_us,
                budget.max_total_overhead_us
            );
        }
    }

    /// **Per-event overhead ≤ max_overhead_us_per_event for
    /// accepted pairs**: every recorded pair has `overhead_us`
    /// within the per-event cap.
    #[test]
    fn accepted_pairs_respect_per_event_cap(
        budget in arb_budget(),
        pairs in arb_pair_sequence(),
    ) {
        init_test_tracing_json();
        let mut h = ShadowExperimentHarness::new(manifest(budget));
        for p in pairs {
            h.record_pair(p);
        }
        for d in &h.ledger().decisions {
            prop_assert!(
                d.overhead_us <= budget.max_overhead_us_per_event,
                "recorded pair overhead {} exceeded per-event cap {}",
                d.overhead_us,
                budget.max_overhead_us_per_event
            );
        }
    }

    /// **Post-abort calls bump sampling_gaps**: after
    /// `is_aborted()` is true, every subsequent `record_pair`
    /// returns false AND increments `sampling_gaps`.
    #[test]
    fn post_abort_calls_bump_sampling_gaps(
        budget in arb_budget(),
        first_batch in arb_pair_sequence(),
        post_abort_count in 0u32..=20,
    ) {
        init_test_tracing_json();
        let mut h = ShadowExperimentHarness::new(manifest(budget));
        for p in first_batch {
            h.record_pair(p);
        }
        if !h.is_aborted() {
            h.abort_manually("force abort for property test");
        }
        let gaps_before = h.ledger().sampling_gaps;
        for i in 0..post_abort_count {
            let pair = ShadowDecisionPair {
                event_id: format!("post-{i}"),
                baseline_decision: "b".to_string(),
                candidate_decision: "c".to_string(),
                divergence: DivergenceKind::Match,
                overhead_us: 100,
                evidence_citations: vec![],
            };
            let recorded = h.record_pair(pair);
            prop_assert!(
                !recorded,
                "post-abort record_pair must return false"
            );
        }
        let gaps_after = h.ledger().sampling_gaps;
        prop_assert_eq!(
            gaps_after - gaps_before,
            u64::from(post_abort_count)
        );
    }

    /// **abort_reason is Some when aborted**: `is_aborted()`
    /// implies `ledger.abort_reason.is_some()`.
    #[test]
    fn aborted_implies_abort_reason_set(
        budget in arb_budget(),
        pairs in arb_pair_sequence(),
        manual_abort in any::<bool>(),
    ) {
        init_test_tracing_json();
        let mut h = ShadowExperimentHarness::new(manifest(budget));
        for p in pairs {
            h.record_pair(p);
        }
        if manual_abort && !h.is_aborted() {
            h.abort_manually("test");
        }
        if h.is_aborted() {
            prop_assert!(h.ledger().abort_reason.is_some());
        }
    }

    /// **abort_manually is idempotent**: calling it twice
    /// preserves the first reason.
    #[test]
    fn abort_manually_is_idempotent(
        budget in arb_budget(),
    ) {
        init_test_tracing_json();
        let mut h = ShadowExperimentHarness::new(manifest(budget));
        h.abort_manually("first");
        h.abort_manually("second");
        h.abort_manually("third");
        let reason = h.ledger().abort_reason.as_ref().expect("reason set");
        let json = serde_json::to_string(reason).expect("serialize");
        prop_assert!(
            json.contains("first"),
            "first abort reason must win; got {json}"
        );
        prop_assert!(
            !json.contains("second"),
            "subsequent abort reasons must not overwrite"
        );
    }

    /// **Counter monotonicity**: every counter only increases
    /// across operations.
    #[test]
    fn counters_are_monotonic(
        budget in arb_budget(),
        pairs in arb_pair_sequence(),
    ) {
        init_test_tracing_json();
        let mut h = ShadowExperimentHarness::new(manifest(budget));
        let mut prev_size = 0u64;
        let mut prev_gaps = 0u64;
        let mut prev_overhead = 0u64;
        for p in pairs {
            h.record_pair(p);
            let l = h.ledger();
            let size = l.decisions.len() as u64;
            prop_assert!(size >= prev_size);
            prop_assert!(l.sampling_gaps >= prev_gaps);
            prop_assert!(l.total_overhead_us >= prev_overhead);
            prev_size = size;
            prev_gaps = l.sampling_gaps;
            prev_overhead = l.total_overhead_us;
        }
    }

    /// **divergence_counts conservation**: `match_count +
    /// minor_count + major_count == decisions.len()`.
    #[test]
    fn divergence_counts_sum_equals_ledger_size(
        budget in arb_budget(),
        pairs in arb_pair_sequence(),
    ) {
        init_test_tracing_json();
        let mut h = ShadowExperimentHarness::new(manifest(budget));
        for p in pairs {
            h.record_pair(p);
        }
        let counts = h.ledger().divergence_counts();
        let total = counts.match_count + counts.minor_count + counts.major_count;
        prop_assert_eq!(total, h.ledger().decisions.len() as u64);
    }

    /// **record_pair return value matches written-state**:
    /// `record_pair(p)` returns true iff `p` is appended to
    /// the `decisions` vec.
    #[test]
    fn record_pair_return_matches_decisions_growth(
        budget in arb_budget(),
        pairs in arb_pair_sequence(),
    ) {
        init_test_tracing_json();
        let mut h = ShadowExperimentHarness::new(manifest(budget));
        for p in pairs {
            let event_id = p.event_id.clone();
            let before = h.ledger().decisions.len();
            let recorded = h.record_pair(p);
            let after = h.ledger().decisions.len();
            if recorded {
                prop_assert_eq!(after, before + 1);
                let last = h.ledger().decisions.last().expect("at least one");
                prop_assert_eq!(&last.event_id, &event_id);
            } else {
                prop_assert_eq!(after, before, "rejected pair must not grow ledger");
            }
        }
    }

    /// **Ledger serde roundtrip**: any ledger state survives
    /// JSON encoding.
    #[test]
    fn ledger_serde_roundtrip(
        budget in arb_budget(),
        pairs in arb_pair_sequence(),
    ) {
        init_test_tracing_json();
        let mut h = ShadowExperimentHarness::new(manifest(budget));
        for p in pairs {
            h.record_pair(p);
        }
        let ledger = h.ledger().clone();
        let json = serde_json::to_string(&ledger).expect("serialize");
        let back: frankenterm_core::shadow_experiment_harness::ExperimentLedger =
            serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(ledger, back);
    }
}
