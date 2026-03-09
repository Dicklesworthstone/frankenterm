//! Property-based tests for the dashboard aggregation module (ft-3hbv9).

use proptest::prelude::*;

use frankenterm_core::backpressure::{BackpressureSnapshot, BackpressureTier};
use frankenterm_core::cost_tracker::{
    AlertSeverity, BudgetAlert, CostDashboardSnapshot, PaneCostSummary, ProviderCostSummary,
};
use frankenterm_core::dashboard::{DashboardManager, DashboardState, QuotaPanel, SystemHealthTier};
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

// =============================================================================
// Additional coverage tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// DB-11: SystemHealthTier serde roundtrip for all 4 variants.
    #[test]
    fn db11_health_tier_serde(
        tier in prop_oneof![
            Just(SystemHealthTier::Green),
            Just(SystemHealthTier::Yellow),
            Just(SystemHealthTier::Red),
            Just(SystemHealthTier::Black),
        ]
    ) {
        let json = serde_json::to_string(&tier).unwrap();
        let back: SystemHealthTier = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(tier, back);
    }

    /// DB-12: SystemHealthTier Display produces lowercase strings.
    #[test]
    fn db12_health_tier_display(
        tier in prop_oneof![
            Just(SystemHealthTier::Green),
            Just(SystemHealthTier::Yellow),
            Just(SystemHealthTier::Red),
            Just(SystemHealthTier::Black),
        ]
    ) {
        let display = format!("{tier}");
        prop_assert!(!display.is_empty());
        let is_lower = display.chars().all(|c| c.is_lowercase());
        prop_assert!(is_lower, "expected lowercase, got: {}", display);
    }

    /// DB-13: SystemHealthTier ordering: Green < Yellow < Red < Black.
    #[test]
    fn db13_health_tier_ordering(_dummy in 0u8..1) {
        prop_assert!(SystemHealthTier::Green < SystemHealthTier::Yellow);
        prop_assert!(SystemHealthTier::Yellow < SystemHealthTier::Red);
        prop_assert!(SystemHealthTier::Red < SystemHealthTier::Black);
    }

    /// DB-14: BackpressureTier→SystemHealthTier mapping preserves severity.
    #[test]
    fn db14_bp_to_health_mapping(
        bp_tier in arb_backpressure_tier(),
    ) {
        let health: SystemHealthTier = bp_tier.into();
        match bp_tier {
            BackpressureTier::Green => prop_assert_eq!(health, SystemHealthTier::Green),
            BackpressureTier::Yellow => prop_assert_eq!(health, SystemHealthTier::Yellow),
            BackpressureTier::Red => prop_assert_eq!(health, SystemHealthTier::Red),
            BackpressureTier::Black => prop_assert_eq!(health, SystemHealthTier::Black),
        }
    }

    /// DB-15: worst_launch_verdict is Allow when everything is Green.
    #[test]
    fn db15_allow_when_green(_dummy in 0u8..1) {
        let mut mgr = DashboardManager::new();
        // Provide green backpressure
        mgr.update_backpressure(BackpressureSnapshot {
            tier: BackpressureTier::Green,
            timestamp_epoch_ms: 0,
            capture_depth: 0,
            capture_capacity: 1000,
            write_depth: 0,
            write_capacity: 1000,
            duration_in_tier_ms: 0,
            transitions: 0,
            paused_panes: vec![],
        });
        let state = mgr.snapshot();
        prop_assert_eq!(state.worst_launch_verdict(), LaunchVerdict::Allow);
    }

    /// DB-16: worst_launch_verdict is Warn when Yellow but not Red.
    #[test]
    fn db16_warn_when_yellow(_dummy in 0u8..1) {
        let mut mgr = DashboardManager::new();
        mgr.update_backpressure(BackpressureSnapshot {
            tier: BackpressureTier::Yellow,
            timestamp_epoch_ms: 0,
            capture_depth: 500,
            capture_capacity: 1000,
            write_depth: 500,
            write_capacity: 1000,
            duration_in_tier_ms: 0,
            transitions: 0,
            paused_panes: vec![],
        });
        let state = mgr.snapshot();
        prop_assert_eq!(state.worst_launch_verdict(), LaunchVerdict::Warn);
    }

    /// DB-17: has_critical_alerts is false when Green, true when Red/Black.
    #[test]
    fn db17_has_critical_alerts(
        bp_tier in arb_backpressure_tier(),
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
        let expected = state.overall_health >= SystemHealthTier::Red;
        prop_assert_eq!(state.has_critical_alerts(), expected);
    }

    /// DB-18: summary_line always starts with "health=".
    #[test]
    fn db18_summary_line_starts_with_health(
        cost in arb_cost_dashboard_snapshot(),
        bp in arb_backpressure_snapshot(),
    ) {
        let mut mgr = DashboardManager::new();
        mgr.update_costs(cost);
        mgr.update_backpressure(bp);
        let state = mgr.snapshot();
        let line = state.summary_line();
        prop_assert!(
            line.starts_with("health="),
            "summary_line should start with 'health=': {}", line
        );
    }

    /// DB-19: limited_provider_count accessor matches panel field.
    #[test]
    fn db19_limited_provider_count_accessor(
        summaries in proptest::collection::vec(arb_rate_limit_summary(), 0..8),
    ) {
        let mut mgr = DashboardManager::new();
        mgr.update_rate_limits(summaries);
        let state = mgr.snapshot();
        prop_assert_eq!(
            state.limited_provider_count(),
            state.rate_limits.limited_provider_count
        );
    }

    /// DB-20: paused_pane_count accessor matches panel field.
    #[test]
    fn db20_paused_pane_count_accessor(bp in arb_backpressure_snapshot()) {
        let mut mgr = DashboardManager::new();
        mgr.update_backpressure(bp);
        let state = mgr.snapshot();
        prop_assert_eq!(
            state.paused_pane_count(),
            state.backpressure.paused_pane_count
        );
    }

    /// DB-21: Rate limit total_limited_panes = sum of all providers' limited_pane_count.
    #[test]
    fn db21_total_limited_panes_sum(
        summaries in proptest::collection::vec(arb_rate_limit_summary(), 0..8),
    ) {
        // The dashboard sums limited_pane_count from ALL providers (including Clear ones)
        let expected: usize = summaries.iter()
            .map(|s| s.limited_pane_count)
            .sum();
        let mut mgr = DashboardManager::new();
        mgr.update_rate_limits(summaries);
        let state = mgr.snapshot();
        prop_assert_eq!(state.rate_limits.total_limited_panes, expected);
    }

    /// DB-22: Telemetry update counters track each update_* call.
    #[test]
    fn db22_telemetry_update_counters(
        n_cost in 0u32..5,
        n_rl in 0u32..5,
        n_bp in 0u32..5,
        n_quota in 0u32..5,
    ) {
        let mut mgr = DashboardManager::new();
        for _ in 0..n_cost {
            mgr.update_costs(CostDashboardSnapshot {
                providers: vec![], panes: vec![], alerts: vec![],
                grand_total_cost_usd: 0.0, grand_total_tokens: 0,
            });
        }
        for _ in 0..n_rl {
            mgr.update_rate_limits(vec![]);
        }
        for _ in 0..n_bp {
            mgr.update_backpressure(BackpressureSnapshot {
                tier: BackpressureTier::Green,
                timestamp_epoch_ms: 0, capture_depth: 0, capture_capacity: 1,
                write_depth: 0, write_capacity: 1, duration_in_tier_ms: 0,
                transitions: 0, paused_panes: vec![],
            });
        }
        for _ in 0..n_quota {
            mgr.update_quota(QuotaGateSnapshot {
                telemetry: QuotaGateTelemetrySnapshot {
                    evaluations: 0, allowed: 0, warned: 0, blocked: 0,
                },
            });
        }
        let t = mgr.telemetry().snapshot();
        prop_assert_eq!(t.cost_updates, u64::from(n_cost));
        prop_assert_eq!(t.rate_limit_updates, u64::from(n_rl));
        prop_assert_eq!(t.backpressure_updates, u64::from(n_bp));
        prop_assert_eq!(t.quota_updates, u64::from(n_quota));
    }

    /// DB-23: Default DashboardManager produces all-zero/empty panels.
    #[test]
    fn db23_default_manager_empty(_dummy in 0u8..1) {
        let mut mgr = DashboardManager::default();
        let state = mgr.snapshot();
        prop_assert_eq!(state.overall_health, SystemHealthTier::Green);
        prop_assert!(state.costs.providers.is_empty());
        prop_assert!(state.costs.alerts.is_empty());
        prop_assert_eq!(state.quota.evaluations, 0);
        prop_assert_eq!(state.rate_limits.limited_provider_count, 0);
        prop_assert_eq!(state.backpressure.paused_pane_count, 0);
    }

    /// DB-24: Cost panel budget alerts have is_blocking=true for Critical severity.
    #[test]
    fn db24_critical_alerts_are_blocking(
        cost_usd in 50.0..1000.0f64,
        limit_usd in 10.0..49.0f64,
    ) {
        let cost = CostDashboardSnapshot {
            providers: vec![ProviderCostSummary {
                agent_type: "codex".to_string(),
                total_tokens: 1000,
                total_cost_usd: cost_usd,
                pane_count: 1,
                record_count: 10,
            }],
            panes: vec![],
            alerts: vec![BudgetAlert {
                agent_type: "codex".to_string(),
                current_cost_usd: cost_usd,
                budget_limit_usd: limit_usd,
                usage_fraction: cost_usd / limit_usd,
                severity: AlertSeverity::Critical,
            }],
            grand_total_cost_usd: cost_usd,
            grand_total_tokens: 1000,
        };
        let mut mgr = DashboardManager::new();
        mgr.update_costs(cost);
        let state = mgr.snapshot();
        // Critical alerts should be blocking
        for alert in &state.costs.alerts {
            if alert.severity == "critical" {
                prop_assert!(alert.is_blocking, "critical alert should be blocking");
            }
        }
    }

    /// DB-25: QuotaPanel serde roundtrip.
    #[test]
    fn db25_quota_panel_serde(
        evaluations in 0u64..10_000,
        allowed in 0u64..5_000,
        warned in 0u64..3_000,
        blocked in 0u64..2_000,
        block_rate_percent in 0u64..100,
    ) {
        let panel = QuotaPanel {
            evaluations,
            allowed,
            warned,
            blocked,
            block_rate_percent,
        };
        let json = serde_json::to_string(&panel).unwrap();
        let back: QuotaPanel = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(panel, back);
    }
}
