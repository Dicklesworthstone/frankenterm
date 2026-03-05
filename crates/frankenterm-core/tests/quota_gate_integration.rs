//! Integration tests for the quota gate pipeline (ft-2dss0).
//!
//! Exercises the full pipeline:
//! CostTracker → budget alerts → QuotaGate.evaluate()
//! RateLimitTracker → provider status → QuotaGate.evaluate()
//! AccountQuotaAdvisory → QuotaGate.evaluate()
//!
//! These tests verify that the three independent subsystems compose correctly
//! through the QuotaGate to produce accurate launch decisions.

use frankenterm_core::accounts::QuotaAvailability;
use frankenterm_core::cost_tracker::{BudgetThreshold, CostTracker, CostTrackerConfig};
use frankenterm_core::patterns::AgentType;
use frankenterm_core::quota_gate::{LaunchVerdict, QuotaGate, QuotaSignals};
use frankenterm_core::rate_limit_tracker::{ProviderRateLimitStatus, ProviderRateLimitSummary};

/// Happy path: no budget pressure, no rate limits, healthy accounts.
#[test]
fn happy_path_all_clear() {
    let config = CostTrackerConfig {
        budgets: vec![BudgetThreshold::new("codex", 100.0, 0.8)],
    };
    let mut cost_tracker = CostTracker::with_config(config);
    // Low usage — well under budget
    cost_tracker.record_usage(1, AgentType::Codex, 5000, 10.0, 100);

    let mut gate = QuotaGate::new();
    let decision = gate.evaluate_from_trackers(
        AgentType::Codex,
        &mut cost_tracker,
        Some(ProviderRateLimitSummary {
            agent_type: "codex".to_string(),
            status: ProviderRateLimitStatus::Clear,
            limited_pane_count: 0,
            total_pane_count: 3,
            earliest_clear_secs: 0,
            total_events: 0,
        }),
        Some(QuotaAvailability::Available),
        Some(85.0),
    );

    assert_eq!(decision.verdict, LaunchVerdict::Allow);
    assert!(decision.warnings.is_empty());
}

/// Budget warning + clear rate limits → Warn (not Block).
#[test]
fn budget_warning_only() {
    let config = CostTrackerConfig {
        budgets: vec![BudgetThreshold::new("codex", 100.0, 0.8)],
    };
    let mut cost_tracker = CostTracker::with_config(config);
    // 90% of budget
    cost_tracker.record_usage(1, AgentType::Codex, 50_000, 90.0, 100);

    let mut gate = QuotaGate::new();
    let decision = gate.evaluate_from_trackers(
        AgentType::Codex,
        &mut cost_tracker,
        Some(ProviderRateLimitSummary {
            agent_type: "codex".to_string(),
            status: ProviderRateLimitStatus::Clear,
            limited_pane_count: 0,
            total_pane_count: 2,
            earliest_clear_secs: 0,
            total_events: 0,
        }),
        Some(QuotaAvailability::Available),
        Some(90.0),
    );

    assert_eq!(decision.verdict, LaunchVerdict::Warn);
    assert_eq!(decision.warnings.len(), 1);
    assert!(!decision.is_blocked());
}

/// Budget exceeded → Block.
#[test]
fn budget_critical_blocks_launch() {
    let config = CostTrackerConfig {
        budgets: vec![BudgetThreshold::new("codex", 50.0, 0.8)],
    };
    let mut cost_tracker = CostTracker::with_config(config);
    // Over budget
    cost_tracker.record_usage(1, AgentType::Codex, 100_000, 65.0, 100);

    let mut gate = QuotaGate::new();
    let decision =
        gate.evaluate_from_trackers(AgentType::Codex, &mut cost_tracker, None, None, None);

    assert_eq!(decision.verdict, LaunchVerdict::Block);
    assert!(decision.is_blocked());
}

/// Rate limit fully limited → Block, even with good budget.
#[test]
fn rate_limit_fully_limited_blocks_despite_good_budget() {
    let config = CostTrackerConfig {
        budgets: vec![BudgetThreshold::new("codex", 1000.0, 0.8)],
    };
    let mut cost_tracker = CostTracker::with_config(config);
    // Low usage
    cost_tracker.record_usage(1, AgentType::Codex, 1000, 5.0, 100);

    let mut gate = QuotaGate::new();
    let decision = gate.evaluate_from_trackers(
        AgentType::Codex,
        &mut cost_tracker,
        Some(ProviderRateLimitSummary {
            agent_type: "codex".to_string(),
            status: ProviderRateLimitStatus::FullyLimited,
            limited_pane_count: 4,
            total_pane_count: 4,
            earliest_clear_secs: 180,
            total_events: 10,
        }),
        Some(QuotaAvailability::Available),
        Some(95.0),
    );

    assert_eq!(decision.verdict, LaunchVerdict::Block);
    assert!(decision.is_blocked());
}

/// Account exhaustion blocks even when budget and rate limits are fine.
#[test]
fn account_exhaustion_blocks_regardless() {
    let mut cost_tracker = CostTracker::new(); // no budget config
    cost_tracker.record_usage(1, AgentType::Codex, 100, 0.01, 100);

    let mut gate = QuotaGate::new();
    let decision = gate.evaluate_from_trackers(
        AgentType::Codex,
        &mut cost_tracker,
        Some(ProviderRateLimitSummary {
            agent_type: "codex".to_string(),
            status: ProviderRateLimitStatus::Clear,
            limited_pane_count: 0,
            total_pane_count: 1,
            earliest_clear_secs: 0,
            total_events: 0,
        }),
        Some(QuotaAvailability::Exhausted),
        None,
    );

    assert_eq!(decision.verdict, LaunchVerdict::Block);
}

/// Combined: budget warning + partial rate limit + low quota → Warn (multiple).
#[test]
fn combined_warnings_all_warn_level() {
    let config = CostTrackerConfig {
        budgets: vec![BudgetThreshold::new("codex", 100.0, 0.8)],
    };
    let mut cost_tracker = CostTracker::with_config(config);
    cost_tracker.record_usage(1, AgentType::Codex, 50_000, 85.0, 100);

    let mut gate = QuotaGate::new();
    let decision = gate.evaluate_from_trackers(
        AgentType::Codex,
        &mut cost_tracker,
        Some(ProviderRateLimitSummary {
            agent_type: "codex".to_string(),
            status: ProviderRateLimitStatus::PartiallyLimited,
            limited_pane_count: 1,
            total_pane_count: 3,
            earliest_clear_secs: 30,
            total_events: 1,
        }),
        Some(QuotaAvailability::Low),
        Some(4.0),
    );

    assert_eq!(decision.verdict, LaunchVerdict::Warn);
    assert_eq!(decision.warnings.len(), 3);
    assert_eq!(decision.block_count(), 0);
}

/// Combined: budget critical + fully limited + exhausted → Block with 3 blocks.
#[test]
fn triple_block_compound() {
    let config = CostTrackerConfig {
        budgets: vec![BudgetThreshold::new("codex", 50.0, 0.8)],
    };
    let mut cost_tracker = CostTracker::with_config(config);
    cost_tracker.record_usage(1, AgentType::Codex, 100_000, 75.0, 100);

    let mut gate = QuotaGate::new();
    let decision = gate.evaluate_from_trackers(
        AgentType::Codex,
        &mut cost_tracker,
        Some(ProviderRateLimitSummary {
            agent_type: "codex".to_string(),
            status: ProviderRateLimitStatus::FullyLimited,
            limited_pane_count: 5,
            total_pane_count: 5,
            earliest_clear_secs: 300,
            total_events: 15,
        }),
        Some(QuotaAvailability::Exhausted),
        None,
    );

    assert_eq!(decision.verdict, LaunchVerdict::Block);
    assert_eq!(decision.block_count(), 3);
}

/// Cross-provider isolation: Codex budget alert doesn't affect ClaudeCode launch.
#[test]
fn cross_provider_budget_isolation() {
    let config = CostTrackerConfig {
        budgets: vec![
            BudgetThreshold::new("codex", 50.0, 0.8),
            BudgetThreshold::new("claude_code", 100.0, 0.8),
        ],
    };
    let mut cost_tracker = CostTracker::with_config(config);
    // Codex over budget
    cost_tracker.record_usage(1, AgentType::Codex, 100_000, 60.0, 100);
    // ClaudeCode well under budget
    cost_tracker.record_usage(2, AgentType::ClaudeCode, 5000, 10.0, 100);

    let mut gate = QuotaGate::new();

    // Codex should be blocked
    let codex_decision =
        gate.evaluate_from_trackers(AgentType::Codex, &mut cost_tracker, None, None, None);
    assert_eq!(codex_decision.verdict, LaunchVerdict::Block);

    // ClaudeCode should be allowed (Codex's budget doesn't affect it)
    let claude_decision =
        gate.evaluate_from_trackers(AgentType::ClaudeCode, &mut cost_tracker, None, None, None);
    assert_eq!(claude_decision.verdict, LaunchVerdict::Allow);
}

/// Telemetry accumulates across multiple evaluations.
#[test]
fn telemetry_accumulates_across_evaluations() {
    let mut cost_tracker = CostTracker::new();
    cost_tracker.record_usage(1, AgentType::Codex, 100, 0.01, 100);

    let mut gate = QuotaGate::new();

    // Allow
    let signals_clear = QuotaSignals::default();
    gate.evaluate(AgentType::Codex, &signals_clear);

    // Warn
    let signals_warn = QuotaSignals {
        quota_availability: Some(QuotaAvailability::Low),
        selected_quota_percent: Some(5.0),
        ..Default::default()
    };
    gate.evaluate(AgentType::Codex, &signals_warn);

    // Block
    let signals_block = QuotaSignals {
        quota_availability: Some(QuotaAvailability::Exhausted),
        ..Default::default()
    };
    gate.evaluate(AgentType::Codex, &signals_block);

    let snap = gate.telemetry().snapshot();
    assert_eq!(snap.evaluations, 3);
    assert_eq!(snap.allowed, 1);
    assert_eq!(snap.warned, 1);
    assert_eq!(snap.blocked, 1);
    assert_eq!(snap.evaluations, snap.allowed + snap.warned + snap.blocked);
}

/// Dashboard snapshot from cost tracker feeds correctly into quota gate.
#[test]
fn cost_dashboard_feeds_into_gate() {
    let config = CostTrackerConfig {
        budgets: vec![
            BudgetThreshold::new("codex", 100.0, 0.8),
            BudgetThreshold::new("gemini", 200.0, 0.9),
        ],
    };
    let mut cost_tracker = CostTracker::with_config(config);

    // Multiple panes, multiple providers
    cost_tracker.record_usage(1, AgentType::Codex, 10_000, 50.0, 100);
    cost_tracker.record_usage(2, AgentType::Codex, 20_000, 40.0, 200);
    cost_tracker.record_usage(3, AgentType::Gemini, 5_000, 15.0, 300);

    // Verify dashboard snapshot is consistent
    let snapshot = cost_tracker.dashboard_snapshot();
    assert_eq!(snapshot.providers.len(), 2);
    assert_eq!(snapshot.panes.len(), 3);
    assert!((snapshot.grand_total_cost_usd - 105.0).abs() < 1e-6);

    // Codex: 90% of $100 budget → warning
    let mut gate = QuotaGate::new();
    let codex_decision =
        gate.evaluate_from_trackers(AgentType::Codex, &mut cost_tracker, None, None, None);
    assert_eq!(codex_decision.verdict, LaunchVerdict::Warn);

    // Gemini: 7.5% of $200 budget → allow
    let gemini_decision =
        gate.evaluate_from_trackers(AgentType::Gemini, &mut cost_tracker, None, None, None);
    assert_eq!(gemini_decision.verdict, LaunchVerdict::Allow);
}
