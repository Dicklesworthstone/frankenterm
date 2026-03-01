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
}
