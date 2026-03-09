//! Property-based tests for the dashboard → TUI adapter pipeline (ft-3hbv9).
//!
//! Validates that `adapt_dashboard()` produces consistent, well-formed view
//! models for arbitrary `DashboardState` inputs.

#[cfg(any(feature = "tui", feature = "ftui"))]
mod tui_tests {
    use proptest::prelude::*;

    use frankenterm_core::backpressure::{BackpressureSnapshot, BackpressureTier};
    use frankenterm_core::cost_tracker::{
        AlertSeverity, BudgetAlert, CostDashboardSnapshot, PaneCostSummary, ProviderCostSummary,
    };
    use frankenterm_core::dashboard::{DashboardManager, SystemHealthTier};
    use frankenterm_core::quota_gate::{QuotaGateSnapshot, QuotaGateTelemetrySnapshot};
    use frankenterm_core::rate_limit_tracker::{ProviderRateLimitStatus, ProviderRateLimitSummary};
    use frankenterm_core::tui::view_adapters::adapt_dashboard;

    // =========================================================================
    // Strategies (reuse from proptest_dashboard.rs patterns)
    // =========================================================================

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
                |(
                    pane_id,
                    agent_type,
                    total_tokens,
                    total_cost_usd,
                    record_count,
                    last_updated_ms,
                )| {
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
            prop_oneof![Just(AlertSeverity::Warning), Just(AlertSeverity::Critical)],
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

    // =========================================================================
    // Property tests
    // =========================================================================

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(50))]

        /// adapt_dashboard never panics for any valid DashboardState.
        #[test]
        fn adapter_never_panics(
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
            let _model = adapt_dashboard(&state);
        }

        /// Health label matches the overall_health tier string.
        #[test]
        fn health_label_matches_tier(
            bp_tier in arb_backpressure_tier(),
        ) {
            let mut mgr = DashboardManager::new();
            mgr.update_backpressure(BackpressureSnapshot {
                tier: bp_tier,
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
            let model = adapt_dashboard(&state);

            let expected_health = SystemHealthTier::from(bp_tier);
            prop_assert_eq!(&model.health_label, &expected_health.to_string());
        }

        /// Cost row count <= provider count (BTreeMap dedup may reduce).
        #[test]
        fn cost_row_count_bounded(
            cost in arb_cost_dashboard_snapshot(),
        ) {
            let input_count = cost.providers.len();
            let mut mgr = DashboardManager::new();
            mgr.update_costs(cost);
            let state = mgr.snapshot();
            let model = adapt_dashboard(&state);
            prop_assert!(model.cost_rows.len() <= input_count);
        }

        /// Rate limit row count equals input provider count.
        #[test]
        fn rate_limit_row_count_matches(
            summaries in proptest::collection::vec(arb_rate_limit_summary(), 0..10),
        ) {
            let input_count = summaries.len();
            let mut mgr = DashboardManager::new();
            mgr.update_rate_limits(summaries);
            let state = mgr.snapshot();
            let model = adapt_dashboard(&state);
            prop_assert_eq!(model.rate_limit_rows.len(), input_count);
        }

        /// Quota block rate label is always a valid percentage string.
        #[test]
        fn quota_block_rate_is_percentage(
            quota in arb_quota_snapshot(),
        ) {
            let mut mgr = DashboardManager::new();
            mgr.update_quota(quota);
            let state = mgr.snapshot();
            let model = adapt_dashboard(&state);
            prop_assert!(
                model.quota_block_rate_label.ends_with('%'),
                "expected % suffix, got: {}",
                model.quota_block_rate_label,
            );
            let pct_str = model.quota_block_rate_label.trim_end_matches('%');
            let pct: u64 = pct_str.parse().expect("valid integer");
            prop_assert!(pct <= 100, "block rate {}% > 100", pct);
        }

        /// Summary line always starts with "health=".
        #[test]
        fn summary_line_starts_with_health(
            cost in arb_cost_dashboard_snapshot(),
            bp in arb_backpressure_snapshot(),
            quota in arb_quota_snapshot(),
        ) {
            let mut mgr = DashboardManager::new();
            mgr.update_costs(cost);
            mgr.update_backpressure(bp);
            mgr.update_quota(quota);
            let state = mgr.snapshot();
            let model = adapt_dashboard(&state);
            prop_assert!(
                model.summary_line.starts_with("health="),
                "summary doesn't start with health=: {}",
                model.summary_line,
            );
        }

        /// Backpressure utilization labels are valid percentages (0-100%).
        #[test]
        fn bp_utilization_valid_percentage(
            bp in arb_backpressure_snapshot(),
        ) {
            let mut mgr = DashboardManager::new();
            mgr.update_backpressure(bp);
            let state = mgr.snapshot();
            let model = adapt_dashboard(&state);

            let cap_pct_str = model.bp_capture_label.trim_end_matches('%');
            let cap_pct: f64 = cap_pct_str.parse().expect("valid float");
            prop_assert!((0.0..=100.0).contains(&cap_pct), "capture {}%", cap_pct);

            let wr_pct_str = model.bp_write_label.trim_end_matches('%');
            let wr_pct: f64 = wr_pct_str.parse().expect("valid float");
            prop_assert!((0.0..=100.0).contains(&wr_pct), "write {}%", wr_pct);
        }

        /// adapt_dashboard is deterministic — same input produces same output.
        #[test]
        fn adapter_deterministic(
            cost in arb_cost_dashboard_snapshot(),
            rate_limits in proptest::collection::vec(arb_rate_limit_summary(), 0..3),
            bp in arb_backpressure_snapshot(),
            quota in arb_quota_snapshot(),
        ) {
            let mut mgr = DashboardManager::new();
            mgr.update_costs(cost.clone());
            mgr.update_rate_limits(rate_limits.clone());
            mgr.update_backpressure(bp.clone());
            mgr.update_quota(quota.clone());
            let state = mgr.snapshot();
            let m1 = adapt_dashboard(&state);
            let m2 = adapt_dashboard(&state);

            prop_assert_eq!(&m1.health_label, &m2.health_label);
            prop_assert_eq!(&m1.total_cost_label, &m2.total_cost_label);
            prop_assert_eq!(&m1.total_tokens_label, &m2.total_tokens_label);
            prop_assert_eq!(&m1.limited_provider_label, &m2.limited_provider_label);
            prop_assert_eq!(&m1.bp_tier_label, &m2.bp_tier_label);
            prop_assert_eq!(&m1.bp_capture_label, &m2.bp_capture_label);
            prop_assert_eq!(&m1.bp_write_label, &m2.bp_write_label);
            prop_assert_eq!(&m1.quota_evaluations_label, &m2.quota_evaluations_label);
            prop_assert_eq!(&m1.quota_block_rate_label, &m2.quota_block_rate_label);
            prop_assert_eq!(&m1.summary_line, &m2.summary_line);
        }
    }
}

/// Placeholder for non-TUI builds (proptest binary must have at least one test).
#[cfg(not(any(feature = "tui", feature = "ftui")))]
#[test]
fn adapter_proptest_requires_tui_feature() {
    // This test file requires `--features tui` or `--features ftui` to exercise
    // the adapter. Without those features, the tui module is not compiled.
}
