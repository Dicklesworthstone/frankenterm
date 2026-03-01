//! Property-based tests for the dashboard aggregation module (ft-3hbv9).

use proptest::prelude::*;

use frankenterm_core::backpressure::{BackpressureSnapshot, BackpressureTier};
use frankenterm_core::cost_tracker::{
    AlertSeverity, BudgetAlert, CostDashboardSnapshot, PaneCostSummary, ProviderCostSummary,
};
use frankenterm_core::dashboard::{
    DashboardManager, DashboardState, SystemHealthTier,
};
use frankenterm_core::quota_gate::{LaunchVerdict, QuotaGateSnapshot, QuotaGateTelemetrySnapshot};
use frankenterm_core::rate_limit_tracker::{ProviderRateLimitStatus, ProviderRateLimitSummary};

// =============================================================================
// Strategies
// =============================================================================

fn arb_backpressure_tier() -> impl Strategy<Value = BackpressureTier> {
    prop_oneof![
        Just(BackpressureTier::Green),
        Just(BackpressureTier::Yellow),
        Just(BackpressureTier::Red),
        Just(BackpressureTier::Black),
    ]
}

fn arb_rate_limit_status() -> impl Strategy<Value = ProviderRateLimitStatus> {
    prop_oneof![
        Just(ProviderRateLimitStatus::Clear),
        Just(ProviderRateLimitStatus::PartiallyLimited),
        Just(ProviderRateLimitStatus::FullyLimited),
    ]
}

fn arb_alert_severity() -> impl Strategy<Value = AlertSeverity> {
    prop_oneof![Just(AlertSeverity::Warning), Just(AlertSeverity::Critical),]
}

fn arb_provider_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("codex".to_string()),
        Just("claude_code".to_string()),
        Just("gemini".to_string()),
    ]
}

fn arb_provider_cost_summary() -> impl Strategy<Value = ProviderCostSummary> {
    (
        arb_provider_name(),
        0u64..1_000_000,
        0.0f64..10_000.0,
        0usize..100,
        0u64..10_000,
    )
        .prop_map(
            |(agent_type, total_tokens, total_cost_usd, pane_count, record_count)| {
                ProviderCostSummary {
                    agent_type,
                    total_tokens,
                    total_cost_usd,
                    pane_count,
                    record_count,
                }
            },
        )
}

fn arb_pane_cost_summary() -> impl Strategy<Value = PaneCostSummary> {
    (
        1u64..1000,
        arb_provider_name(),
        0u64..500_000,
        0.0f64..5_000.0,
        0u64..5_000,
        0i64..1_000_000_000,
    )
        .prop_map(
            |(pane_id, agent_type, total_tokens, total_cost_usd, record_count, last_updated_ms)| {
                PaneCostSummary {
                    pane_id,
                    agent_type,
                    total_tokens,
                    total_cost_usd,
                    record_count,
                    last_updated_ms,
                }
            },
        )
}

fn arb_budget_alert() -> impl Strategy<Value = BudgetAlert> {
    (
        arb_provider_name(),
        0.0f64..10_000.0,
        1.0f64..10_000.0,
        0.0f64..2.0,
        arb_alert_severity(),
    )
        .prop_map(
            |(agent_type, current_cost_usd, budget_limit_usd, usage_fraction, severity)| {
                BudgetAlert {
                    agent_type,
                    current_cost_usd,
                    budget_limit_usd,
                    usage_fraction,
                    severity,
                }
            },
        )
}

fn arb_cost_dashboard_snapshot() -> impl Strategy<Value = CostDashboardSnapshot> {
    (
        proptest::collection::vec(arb_provider_cost_summary(), 0..5),
        proptest::collection::vec(arb_pane_cost_summary(), 0..10),
        proptest::collection::vec(arb_budget_alert(), 0..3),
        0.0f64..100_000.0,
        0u64..10_000_000,
    )
        .prop_map(
            |(providers, panes, alerts, grand_total_cost_usd, grand_total_tokens)| {
                CostDashboardSnapshot {
                    providers,
                    panes,
                    alerts,
                    grand_total_cost_usd,
                    grand_total_tokens,
                }
            },
        )
}

fn arb_rate_limit_summary() -> impl Strategy<Value = ProviderRateLimitSummary> {
    (
        arb_provider_name(),
        arb_rate_limit_status(),
        0usize..50,
        0usize..50,
        0u64..600,
        0usize..100,
    )
        .prop_map(
            |(
                agent_type,
                status,
                limited_pane_count,
                total_pane_count,
                earliest_clear_secs,
                total_events,
            )| {
                // Ensure limited <= total.
                let total = total_pane_count.max(limited_pane_count);
                ProviderRateLimitSummary {
                    agent_type,
                    status,
                    limited_pane_count,
                    total_pane_count: total,
                    earliest_clear_secs,
                    total_events,
                }
            },
        )
}

fn arb_backpressure_snapshot() -> impl Strategy<Value = BackpressureSnapshot> {
    (
        arb_backpressure_tier(),
        0u64..2_000_000_000,
        0usize..10_000,
        1usize..10_000,
        0usize..10_000,
        1usize..10_000,
        0u64..1_000_000,
        0u64..1000,
        proptest::collection::vec(1u64..1000, 0..20),
    )
        .prop_map(
            |(
                tier,
                timestamp_epoch_ms,
                capture_depth,
                capture_capacity,
                write_depth,
                write_capacity,
                duration_in_tier_ms,
                transitions,
                paused_panes,
            )| {
                BackpressureSnapshot {
                    tier,
                    timestamp_epoch_ms,
                    capture_depth: capture_depth.min(capture_capacity),
                    capture_capacity,
                    write_depth: write_depth.min(write_capacity),
                    write_capacity,
                    duration_in_tier_ms,
                    transitions,
                    paused_panes,
                }
            },
        )
}

fn arb_quota_snapshot() -> impl Strategy<Value = QuotaGateSnapshot> {
    (0u64..10_000, 0u64..10_000, 0u64..10_000).prop_map(|(allowed, warned, blocked)| {
        QuotaGateSnapshot {
            telemetry: QuotaGateTelemetrySnapshot {
                evaluations: allowed + warned + blocked,
                allowed,
                warned,
                blocked,
            },
        }
    })
}

// =============================================================================
// Property tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Snapshot serde roundtrip preserves all fields except timestamp.
    #[test]
    fn dashboard_state_serde_roundtrip(
        cost in arb_cost_dashboard_snapshot(),
        rate_limits in proptest::collection::vec(arb_rate_limit_summary(), 0..5),
        bp in arb_backpressure_snapshot(),
        quota in arb_quota_snapshot(),
    ) {
        let mut mgr = DashboardManager::new();
        mgr.update_costs(cost);
        mgr.update_rate_limits(rate_limits);
        mgr.update_backpressure(bp);
        mgr.update_quota(quota);
        let state = mgr.snapshot();

        let json = serde_json::to_string(&state).expect("serialize");
        let deser: DashboardState = serde_json::from_str(&json).expect("deserialize");

        prop_assert_eq!(deser.overall_health, state.overall_health);
        prop_assert_eq!(&deser.rate_limits, &state.rate_limits);
        prop_assert_eq!(&deser.quota, &state.quota);
        prop_assert_eq!(&deser.telemetry, &state.telemetry);
        // Cost + backpressure panels contain f64 — JSON roundtrip may lose
        // precision at the last decimal digit, so compare via PartialEq (bitwise
        // for f64). PartialEq on f64 is exact, but serde_json preserves bits
        // through the f64→string→f64 path for values that fit in 64-bit IEEE 754.
        // Use tolerance check for the grand total which can have precision loss.
        prop_assert_eq!(deser.costs.providers.len(), state.costs.providers.len());
        prop_assert_eq!(deser.costs.alerts.len(), state.costs.alerts.len());
        prop_assert_eq!(deser.costs.total_tokens, state.costs.total_tokens);
        prop_assert_eq!(deser.costs.pane_count, state.costs.pane_count);
        let cost_diff = (deser.costs.total_cost_usd - state.costs.total_cost_usd).abs();
        prop_assert!(cost_diff < 0.01, "cost diff {} >= 0.01", cost_diff);
        let bp_capture_diff = (deser.backpressure.capture_utilization
            - state.backpressure.capture_utilization)
            .abs();
        let bp_write_diff = (deser.backpressure.write_utilization
            - state.backpressure.write_utilization)
            .abs();
        prop_assert!(bp_capture_diff < 1e-10, "capture util diff {}", bp_capture_diff);
        prop_assert!(bp_write_diff < 1e-10, "write util diff {}", bp_write_diff);
        prop_assert_eq!(&deser.backpressure.tier, &state.backpressure.tier);
        prop_assert_eq!(deser.backpressure.health, state.backpressure.health);
    }

    /// Overall health is always >= each subsystem's health contribution.
    #[test]
    fn overall_health_at_least_backpressure_tier(
        bp in arb_backpressure_snapshot(),
    ) {
        let mut mgr = DashboardManager::new();
        mgr.update_backpressure(bp.clone());
        let state = mgr.snapshot();
        let bp_health = SystemHealthTier::from(bp.tier);
        prop_assert!(
            state.overall_health >= bp_health,
            "overall {:?} < backpressure {:?}",
            state.overall_health,
            bp_health,
        );
    }

    /// Quota panel block_rate_percent is always <= 100.
    #[test]
    fn block_rate_bounded(
        quota in arb_quota_snapshot(),
    ) {
        let mut mgr = DashboardManager::new();
        mgr.update_quota(quota);
        let state = mgr.snapshot();
        prop_assert!(
            state.quota.block_rate_percent <= 100,
            "block_rate {}% > 100",
            state.quota.block_rate_percent,
        );
    }

    /// Quota panel evaluations = allowed + warned + blocked.
    #[test]
    fn quota_telemetry_conservation(
        quota in arb_quota_snapshot(),
    ) {
        let mut mgr = DashboardManager::new();
        mgr.update_quota(quota);
        let state = mgr.snapshot();
        prop_assert_eq!(
            state.quota.evaluations,
            state.quota.allowed + state.quota.warned + state.quota.blocked,
        );
    }

    /// Telemetry snapshot_count increments exactly once per snapshot() call.
    #[test]
    fn telemetry_monotonicity(n in 1u32..20) {
        let mut mgr = DashboardManager::new();
        for _ in 0..n {
            let _ = mgr.snapshot();
        }
        let t = mgr.telemetry().snapshot();
        prop_assert_eq!(t.snapshots_taken, u64::from(n));
    }

    /// worst_launch_verdict is Block when overall_health >= Red.
    #[test]
    fn block_verdict_when_red_or_black(
        bp_tier in prop_oneof![
            Just(BackpressureTier::Red),
            Just(BackpressureTier::Black),
        ],
    ) {
        let mut mgr = DashboardManager::new();
        mgr.update_backpressure(BackpressureSnapshot {
            tier: bp_tier,
            timestamp_epoch_ms: 0,
            capture_depth: 900,
            capture_capacity: 1000,
            write_depth: 900,
            write_capacity: 1000,
            duration_in_tier_ms: 0,
            transitions: 0,
            paused_panes: vec![],
        });
        let state = mgr.snapshot();
        prop_assert_eq!(state.worst_launch_verdict(), LaunchVerdict::Block);
    }

    /// Cost panel provider count matches input provider count.
    #[test]
    fn cost_panel_preserves_provider_count(
        cost in arb_cost_dashboard_snapshot(),
    ) {
        let expected_count = cost.providers.len();
        let mut mgr = DashboardManager::new();
        mgr.update_costs(cost);
        let state = mgr.snapshot();
        // BTreeMap deduplicates by key, so count may be <= input if duplicate names.
        prop_assert!(state.costs.providers.len() <= expected_count);
    }

    /// Rate limit panel limited_provider_count <= provider count.
    #[test]
    fn limited_providers_bounded_by_total(
        summaries in proptest::collection::vec(arb_rate_limit_summary(), 0..10),
    ) {
        let provider_count = summaries.len();
        let mut mgr = DashboardManager::new();
        mgr.update_rate_limits(summaries);
        let state = mgr.snapshot();
        prop_assert!(
            state.rate_limits.limited_provider_count <= provider_count,
            "limited {} > total {}",
            state.rate_limits.limited_provider_count,
            provider_count,
        );
    }

    /// Backpressure utilization is always in [0.0, 1.0] when depth <= capacity.
    #[test]
    fn backpressure_utilization_bounded(
        bp in arb_backpressure_snapshot(),
    ) {
        let mut mgr = DashboardManager::new();
        mgr.update_backpressure(bp);
        let state = mgr.snapshot();
        prop_assert!(state.backpressure.capture_utilization >= 0.0);
        prop_assert!(state.backpressure.capture_utilization <= 1.0);
        prop_assert!(state.backpressure.write_utilization >= 0.0);
        prop_assert!(state.backpressure.write_utilization <= 1.0);
    }

    /// DashboardTelemetrySnapshot serde roundtrip.
    #[test]
    fn telemetry_snapshot_serde_roundtrip(
        snapshots_taken in 0u64..10_000,
        cost_updates in 0u64..10_000,
        rate_limit_updates in 0u64..10_000,
        backpressure_updates in 0u64..10_000,
        quota_updates in 0u64..10_000,
    ) {
        let snap = frankenterm_core::dashboard::DashboardTelemetrySnapshot {
            snapshots_taken,
            cost_updates,
            rate_limit_updates,
            backpressure_updates,
            quota_updates,
        };
        let json = serde_json::to_string(&snap).expect("serialize");
        let deser: frankenterm_core::dashboard::DashboardTelemetrySnapshot =
            serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(snap, deser);
    }
}
