//! Property-based tests for cost_tracker module (ft-2dss0).
//!
//! Invariants tested:
//! 1. Cost additivity: total cost equals sum of per-pane costs
//! 2. Token additivity: total tokens equals sum of per-pane tokens
//! 3. Provider isolation: costs for one provider don't appear in another's summary
//! 4. Bounded capacity: tracked panes never exceed MAX_TRACKED_PANES (512)
//! 5. Budget alert consistency: critical severity only when usage >= budget
//! 6. Serde roundtrip: dashboard snapshot survives JSON serialization
//! 7. Removal completeness: remove_pane fully clears state
//! 8. Telemetry monotonicity: counters never decrease

use frankenterm_core::cost_tracker::{
    AlertSeverity, BudgetThreshold, CostDashboardSnapshot, CostTelemetrySnapshot, CostTracker,
    CostTrackerConfig, PaneCostSummary,
};
use frankenterm_core::patterns::AgentType;
use proptest::prelude::*;

/// Strategy for agent types.
fn arb_agent_type() -> impl Strategy<Value = AgentType> {
    prop_oneof![
        Just(AgentType::Codex),
        Just(AgentType::ClaudeCode),
        Just(AgentType::Gemini),
    ]
}

/// Strategy for pane IDs.
fn arb_pane_id() -> impl Strategy<Value = u64> {
    0u64..500
}

/// Strategy for token counts.
fn arb_tokens() -> impl Strategy<Value = u64> {
    0u64..1_000_000
}

/// Strategy for cost in USD.
fn arb_cost_usd() -> impl Strategy<Value = f64> {
    (0.0f64..100.0).prop_map(|v| (v * 10000.0).round() / 10000.0)
}

/// Strategy for timestamps (epoch ms).
fn arb_timestamp_ms() -> impl Strategy<Value = i64> {
    1_700_000_000_000i64..1_800_000_000_000
}

/// Strategy for a single usage record (pane_id, agent_type, tokens, cost_usd, at_ms).
fn arb_usage_record() -> impl Strategy<Value = (u64, AgentType, u64, f64, i64)> {
    (
        arb_pane_id(),
        arb_agent_type(),
        arb_tokens(),
        arb_cost_usd(),
        arb_timestamp_ms(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Invariant 1: Grand total cost equals sum of per-pane costs.
    #[test]
    fn cost_additivity_across_panes(
        records in proptest::collection::vec(arb_usage_record(), 1..50)
    ) {
        let mut tracker = CostTracker::new();
        for (pane_id, agent_type, tokens, cost, ts) in &records {
            tracker.record_usage(*pane_id, *agent_type, *tokens, *cost, *ts);
        }

        let pane_summaries = tracker.all_pane_summaries();
        let sum_of_panes: f64 = pane_summaries.iter().map(|s| s.total_cost_usd).sum();
        let grand_total = tracker.grand_total_cost();

        // f64 tolerance
        prop_assert!((sum_of_panes - grand_total).abs() < 1e-6,
            "sum_of_panes={}, grand_total={}", sum_of_panes, grand_total);
    }

    /// Invariant 2: Grand total tokens equals sum of per-pane tokens.
    #[test]
    fn token_additivity_across_panes(
        records in proptest::collection::vec(arb_usage_record(), 1..50)
    ) {
        let mut tracker = CostTracker::new();
        for (pane_id, agent_type, tokens, cost, ts) in &records {
            tracker.record_usage(*pane_id, *agent_type, *tokens, *cost, *ts);
        }

        let pane_summaries = tracker.all_pane_summaries();
        let sum_of_panes: u64 = pane_summaries.iter().map(|s| s.total_tokens).sum();
        let grand_total = tracker.grand_total_tokens();

        prop_assert_eq!(sum_of_panes, grand_total);
    }

    /// Invariant 3: Provider summary tokens match sum of matching pane tokens.
    #[test]
    fn provider_summary_matches_pane_sums(
        records in proptest::collection::vec(arb_usage_record(), 1..50)
    ) {
        let mut tracker = CostTracker::new();
        for (pane_id, agent_type, tokens, cost, ts) in &records {
            tracker.record_usage(*pane_id, *agent_type, *tokens, *cost, *ts);
        }

        let provider_summaries = tracker.all_provider_summaries();
        let pane_summaries = tracker.all_pane_summaries();

        for ps in &provider_summaries {
            let matching_panes: Vec<&PaneCostSummary> = pane_summaries
                .iter()
                .filter(|p| p.agent_type == ps.agent_type)
                .collect();

            let pane_token_sum: u64 = matching_panes.iter().map(|p| p.total_tokens).sum();
            let pane_cost_sum: f64 = matching_panes.iter().map(|p| p.total_cost_usd).sum();

            prop_assert_eq!(ps.total_tokens, pane_token_sum,
                "provider {} token mismatch", ps.agent_type);
            prop_assert!((ps.total_cost_usd - pane_cost_sum).abs() < 1e-6,
                "provider {} cost mismatch: {} vs {}", ps.agent_type, ps.total_cost_usd, pane_cost_sum);
            prop_assert_eq!(ps.pane_count, matching_panes.len());
        }
    }

    /// Invariant 4: Bounded capacity — never more than 512 tracked panes.
    #[test]
    fn bounded_pane_capacity(
        records in proptest::collection::vec(arb_usage_record(), 1..600)
    ) {
        let mut tracker = CostTracker::new();
        for (pane_id, agent_type, tokens, cost, ts) in &records {
            tracker.record_usage(*pane_id, *agent_type, *tokens, *cost, *ts);
        }
        prop_assert!(tracker.tracked_pane_count() <= 512);
    }

    /// Invariant 5: Budget alerts have correct severity classification.
    #[test]
    fn budget_alert_severity_consistency(
        budget_limit in 1.0f64..1000.0,
        warning_fraction in 0.1f64..1.0,
        cost in 0.0f64..2000.0,
    ) {
        let config = CostTrackerConfig {
            budgets: vec![BudgetThreshold::new("codex", budget_limit, warning_fraction)],
        };
        let mut tracker = CostTracker::with_config(config);
        let cost_rounded = (cost * 10000.0).round() / 10000.0;
        tracker.record_usage(1, AgentType::Codex, 1000, cost_rounded, 100);

        let alerts = tracker.budget_alerts();

        let fraction = cost_rounded / budget_limit;
        if fraction >= 1.0 {
            prop_assert!(alerts.iter().any(|a| a.severity == AlertSeverity::Critical),
                "Expected critical alert for fraction={}", fraction);
        } else if fraction >= warning_fraction {
            prop_assert!(alerts.iter().any(|a| a.severity == AlertSeverity::Warning),
                "Expected warning alert for fraction={}, threshold={}", fraction, warning_fraction);
        } else {
            prop_assert!(alerts.is_empty(),
                "Expected no alerts for fraction={}, threshold={}", fraction, warning_fraction);
        }
    }

    /// Invariant 6: Dashboard snapshot survives JSON roundtrip.
    #[test]
    fn dashboard_serde_roundtrip(
        records in proptest::collection::vec(arb_usage_record(), 1..20)
    ) {
        let config = CostTrackerConfig {
            budgets: vec![
                BudgetThreshold::new("codex", 50.0, 0.8),
                BudgetThreshold::new("claude_code", 100.0, 0.9),
            ],
        };
        let mut tracker = CostTracker::with_config(config);
        for (pane_id, agent_type, tokens, cost, ts) in &records {
            tracker.record_usage(*pane_id, *agent_type, *tokens, *cost, *ts);
        }

        let snapshot = tracker.dashboard_snapshot();
        let json = serde_json::to_string(&snapshot).unwrap();
        let deserialized: CostDashboardSnapshot = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(snapshot.providers.len(), deserialized.providers.len());
        prop_assert_eq!(snapshot.panes.len(), deserialized.panes.len());
        prop_assert_eq!(snapshot.alerts.len(), deserialized.alerts.len());
        prop_assert!((snapshot.grand_total_cost_usd - deserialized.grand_total_cost_usd).abs() < 1e-10);
        prop_assert_eq!(snapshot.grand_total_tokens, deserialized.grand_total_tokens);
    }

    /// Invariant 7: remove_pane fully clears pane state.
    #[test]
    fn remove_pane_clears_completely(
        pane_id in arb_pane_id(),
        records in proptest::collection::vec(
            (arb_tokens(), arb_cost_usd(), arb_timestamp_ms()),
            1..10,
        ),
    ) {
        let mut tracker = CostTracker::new();
        for (tokens, cost, ts) in &records {
            tracker.record_usage(pane_id, AgentType::Codex, *tokens, *cost, *ts);
        }
        prop_assert!(tracker.pane_summary(pane_id).is_some());

        tracker.remove_pane(pane_id);
        prop_assert!(tracker.pane_summary(pane_id).is_none());
        prop_assert_eq!(tracker.tracked_pane_count(), 0);
    }

    /// Invariant 8: Telemetry counters are monotonically non-decreasing.
    #[test]
    fn telemetry_monotonicity(
        records in proptest::collection::vec(arb_usage_record(), 2..30)
    ) {
        let mut tracker = CostTracker::new();
        let mut prev_recorded = 0u64;

        for (pane_id, agent_type, tokens, cost, ts) in &records {
            tracker.record_usage(*pane_id, *agent_type, *tokens, *cost, *ts);
            let snap = tracker.telemetry().snapshot();
            prop_assert!(snap.usages_recorded >= prev_recorded,
                "usages_recorded decreased: {} < {}", snap.usages_recorded, prev_recorded);
            prev_recorded = snap.usages_recorded;
        }
    }

    /// Invariant 9: Telemetry snapshot serde roundtrip.
    #[test]
    fn telemetry_serde_roundtrip(
        usages in 0u64..1000,
        evictions in 0u64..100,
        removals in 0u64..100,
        evals in 0u64..500,
        triggers in 0u64..100,
    ) {
        let snap = CostTelemetrySnapshot {
            usages_recorded: usages,
            panes_evicted_lru: evictions,
            panes_removed: removals,
            alert_evaluations: evals,
            alerts_triggered: triggers,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: CostTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, deserialized);
    }

    // =========================================================================
    // CT-10: warning_fraction is always clamped to [0.0, 1.0]
    // =========================================================================

    #[test]
    fn ct10_warning_fraction_clamped(
        agent_type in "[a-z]{3,10}",
        max_cost in 1.0f64..1000.0,
        raw_fraction in -2.0f64..3.0,
    ) {
        let threshold = BudgetThreshold::new(agent_type, max_cost, raw_fraction);
        prop_assert!(threshold.warning_fraction >= 0.0);
        prop_assert!(threshold.warning_fraction <= 1.0);
    }

    // =========================================================================
    // CT-11: LRU eviction preserves most-recently-used panes
    // =========================================================================

    #[test]
    fn ct11_lru_eviction_keeps_recent(
        recent_pane_id in 600u64..700,
    ) {
        let mut tracker = CostTracker::new();
        // Fill to capacity with pane IDs 0..512
        for i in 0..512u64 {
            tracker.record_usage(i, AgentType::Codex, 10, 0.001, i as i64);
        }
        // Touch the recent pane — it should survive eviction
        tracker.record_usage(recent_pane_id, AgentType::Codex, 10, 0.001, 1000);
        prop_assert!(tracker.pane_summary(recent_pane_id).is_some(),
            "Recently added pane should survive");
        prop_assert!(tracker.tracked_pane_count() <= 512);
    }

    // =========================================================================
    // CT-12: Pane summaries are always sorted by pane_id (BTreeMap guarantee)
    // =========================================================================

    #[test]
    fn ct12_pane_summaries_sorted(
        records in proptest::collection::vec(arb_usage_record(), 2..30)
    ) {
        let mut tracker = CostTracker::new();
        for (pane_id, agent_type, tokens, cost, ts) in &records {
            tracker.record_usage(*pane_id, *agent_type, *tokens, *cost, *ts);
        }
        let summaries = tracker.all_pane_summaries();
        let is_sorted = summaries.windows(2).all(|w| w[0].pane_id <= w[1].pane_id);
        prop_assert!(is_sorted, "Pane summaries should be sorted by pane_id");
    }

    // =========================================================================
    // CT-13: Re-registering a pane with different agent_type updates the type
    // =========================================================================

    #[test]
    fn ct13_agent_type_update_on_reuse(
        pane_id in arb_pane_id(),
        first_type in arb_agent_type(),
        second_type in arb_agent_type(),
        tokens1 in arb_tokens(),
        tokens2 in arb_tokens(),
        cost1 in arb_cost_usd(),
        cost2 in arb_cost_usd(),
    ) {
        let mut tracker = CostTracker::new();
        tracker.record_usage(pane_id, first_type, tokens1, cost1, 100);
        tracker.record_usage(pane_id, second_type, tokens2, cost2, 200);

        let summary = tracker.pane_summary(pane_id).unwrap();
        // Agent type should be the most recent
        prop_assert_eq!(summary.agent_type, second_type.to_string());
        // Tokens accumulate across both
        prop_assert_eq!(summary.total_tokens, tokens1.saturating_add(tokens2));
    }

    // =========================================================================
    // CT-14: record_count tracks number of record_usage calls per pane
    // =========================================================================

    #[test]
    fn ct14_record_count_per_pane(
        pane_id in arb_pane_id(),
        n in 1usize..20,
    ) {
        let mut tracker = CostTracker::new();
        for i in 0..n {
            tracker.record_usage(pane_id, AgentType::Codex, 100, 0.01, i as i64);
        }
        let summary = tracker.pane_summary(pane_id).unwrap();
        prop_assert_eq!(summary.record_count, n as u64);
    }

    // =========================================================================
    // CT-15: last_updated_ms tracks the maximum timestamp
    // =========================================================================

    #[test]
    fn ct15_last_updated_is_max_timestamp(
        pane_id in arb_pane_id(),
        timestamps in proptest::collection::vec(arb_timestamp_ms(), 2..10),
    ) {
        let mut tracker = CostTracker::new();
        for &ts in &timestamps {
            tracker.record_usage(pane_id, AgentType::Codex, 100, 0.01, ts);
        }
        let summary = tracker.pane_summary(pane_id).unwrap();
        let max_ts = *timestamps.iter().max().unwrap();
        prop_assert_eq!(summary.last_updated_ms, max_ts);
    }

    // =========================================================================
    // CT-16: Zero budget produces no alerts regardless of cost
    // =========================================================================

    #[test]
    fn ct16_zero_budget_no_alerts(cost in arb_cost_usd()) {
        let config = CostTrackerConfig {
            budgets: vec![BudgetThreshold::new("codex", 0.0, 0.8)],
        };
        let mut tracker = CostTracker::with_config(config);
        tracker.record_usage(1, AgentType::Codex, 1000, cost, 100);
        let alerts = tracker.budget_alerts();
        prop_assert!(alerts.is_empty(), "Zero budget should never trigger alerts");
    }

    // =========================================================================
    // CT-17: Grand total cost equals sum of all provider summaries
    // =========================================================================

    #[test]
    fn ct17_grand_total_equals_provider_sum(
        records in proptest::collection::vec(arb_usage_record(), 1..30)
    ) {
        let mut tracker = CostTracker::new();
        for (pane_id, agent_type, tokens, cost, ts) in &records {
            tracker.record_usage(*pane_id, *agent_type, *tokens, *cost, *ts);
        }

        let provider_cost_sum: f64 = tracker.all_provider_summaries()
            .iter()
            .map(|p| p.total_cost_usd)
            .sum();
        let grand_total = tracker.grand_total_cost();

        prop_assert!((provider_cost_sum - grand_total).abs() < 1e-6,
            "provider sum {} != grand total {}", provider_cost_sum, grand_total);
    }

    // =========================================================================
    // CT-18: Grand total tokens equals sum of all provider summary tokens
    // =========================================================================

    #[test]
    fn ct18_grand_total_tokens_equals_provider_sum(
        records in proptest::collection::vec(arb_usage_record(), 1..30)
    ) {
        let mut tracker = CostTracker::new();
        for (pane_id, agent_type, tokens, cost, ts) in &records {
            tracker.record_usage(*pane_id, *agent_type, *tokens, *cost, *ts);
        }

        let provider_token_sum: u64 = tracker.all_provider_summaries()
            .iter()
            .map(|p| p.total_tokens)
            .sum();
        let grand_total = tracker.grand_total_tokens();

        prop_assert_eq!(provider_token_sum, grand_total);
    }

    // =========================================================================
    // CT-19: set_config dynamically changes alert behavior
    // =========================================================================

    #[test]
    fn ct19_set_config_changes_alerts(
        cost in 5.0f64..50.0,
    ) {
        let mut tracker = CostTracker::new();
        tracker.record_usage(1, AgentType::Codex, 1000, cost, 100);

        // No config → no alerts
        let alerts1 = tracker.budget_alerts();
        prop_assert!(alerts1.is_empty());

        // Set config with limit below cost → should trigger
        tracker.set_config(CostTrackerConfig {
            budgets: vec![BudgetThreshold::new("codex", cost * 0.5, 0.5)],
        });
        let alerts2 = tracker.budget_alerts();
        prop_assert!(!alerts2.is_empty(),
            "Should trigger alert when cost {} exceeds limit {}", cost, cost * 0.5);
    }

    // =========================================================================
    // CT-20: usages_recorded equals total number of record_usage calls
    // =========================================================================

    #[test]
    fn ct20_usages_recorded_matches_calls(
        records in proptest::collection::vec(arb_usage_record(), 1..50)
    ) {
        let mut tracker = CostTracker::new();
        for (pane_id, agent_type, tokens, cost, ts) in &records {
            tracker.record_usage(*pane_id, *agent_type, *tokens, *cost, *ts);
        }
        prop_assert_eq!(tracker.telemetry().snapshot().usages_recorded, records.len() as u64);
    }

    // =========================================================================
    // CT-21: AlertSeverity serde roundtrip
    // =========================================================================

    #[test]
    fn ct21_alert_severity_serde(
        severity in prop_oneof![
            Just(AlertSeverity::Warning),
            Just(AlertSeverity::Critical),
        ]
    ) {
        let json = serde_json::to_string(&severity).unwrap();
        let back: AlertSeverity = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(severity, back);
    }

    // =========================================================================
    // CT-22: Remove then re-add a pane starts fresh (no cost carryover)
    // =========================================================================

    #[test]
    fn ct22_remove_readd_starts_fresh(
        pane_id in arb_pane_id(),
        cost1 in 1.0f64..50.0,
        cost2 in 1.0f64..50.0,
    ) {
        let mut tracker = CostTracker::new();
        tracker.record_usage(pane_id, AgentType::Codex, 1000, cost1, 100);
        tracker.remove_pane(pane_id);
        tracker.record_usage(pane_id, AgentType::Codex, 500, cost2, 200);

        let summary = tracker.pane_summary(pane_id).unwrap();
        // Should only reflect the second recording
        prop_assert_eq!(summary.total_tokens, 500);
        prop_assert!((summary.total_cost_usd - cost2).abs() < 1e-10);
        prop_assert_eq!(summary.record_count, 1);
    }
}
