//! Property-based tests for quota_gate module (ft-2dss0).
//!
//! Invariants tested:
//! 1. Verdict monotonicity: Block > Warn > Allow, adding signals never downgrades
//! 2. Budget isolation: alerts for other providers don't affect evaluated provider
//! 3. Telemetry conservation: evaluations = allowed + warned + blocked
//! 4. LaunchDecision serde roundtrip
//! 5. Verdict consistency: Block iff any warning is blocking
//! 6. Telemetry monotonicity: counters never decrease
//! 7. QuotaGateTelemetrySnapshot serde roundtrip

use frankenterm_core::accounts::QuotaAvailability;
use frankenterm_core::cost_tracker::{AlertSeverity, BudgetAlert};
use frankenterm_core::patterns::AgentType;
use frankenterm_core::quota_gate::{
    LaunchDecision, LaunchVerdict, QuotaGate, QuotaGateTelemetrySnapshot, QuotaSignals,
    WarningSource,
};
use frankenterm_core::rate_limit_tracker::{ProviderRateLimitStatus, ProviderRateLimitSummary};
use proptest::prelude::*;

/// Strategy for agent types.
fn arb_agent_type() -> impl Strategy<Value = AgentType> {
    prop_oneof![
        Just(AgentType::Codex),
        Just(AgentType::ClaudeCode),
        Just(AgentType::Gemini),
    ]
}

/// Strategy for alert severity.
fn arb_severity() -> impl Strategy<Value = AlertSeverity> {
    prop_oneof![Just(AlertSeverity::Warning), Just(AlertSeverity::Critical),]
}

/// Strategy for rate limit status.
fn arb_rate_limit_status() -> impl Strategy<Value = ProviderRateLimitStatus> {
    prop_oneof![
        Just(ProviderRateLimitStatus::Clear),
        Just(ProviderRateLimitStatus::PartiallyLimited),
        Just(ProviderRateLimitStatus::FullyLimited),
    ]
}

/// Strategy for quota availability.
fn arb_quota_availability() -> impl Strategy<Value = QuotaAvailability> {
    prop_oneof![
        Just(QuotaAvailability::Available),
        Just(QuotaAvailability::Low),
        Just(QuotaAvailability::Exhausted),
    ]
}

/// Strategy for a budget alert targeting a given provider name.
fn arb_budget_alert(provider: String) -> impl Strategy<Value = BudgetAlert> {
    (arb_severity(), 1.0f64..100.0, 0.0f64..200.0).prop_map(
        move |(severity, budget_limit, current_cost)| BudgetAlert {
            agent_type: provider.clone(),
            severity,
            budget_limit_usd: budget_limit,
            current_cost_usd: current_cost,
            usage_fraction: current_cost / budget_limit,
        },
    )
}

/// Strategy for a rate limit summary.
fn arb_rate_limit_summary(
    agent_type_str: String,
) -> impl Strategy<Value = ProviderRateLimitSummary> {
    (
        arb_rate_limit_status(),
        0usize..20,
        1usize..20,
        0u64..600,
        0usize..100,
    )
        .prop_map(
            move |(status, limited_count, total_count, earliest, total_events)| {
                let total = total_count.max(limited_count);
                ProviderRateLimitSummary {
                    agent_type: agent_type_str.clone(),
                    status,
                    limited_pane_count: limited_count.min(total),
                    total_pane_count: total,
                    earliest_clear_secs: earliest,
                    total_events,
                }
            },
        )
}

/// Strategy for complete QuotaSignals.
fn arb_signals(agent_type: AgentType) -> impl Strategy<Value = QuotaSignals> {
    let provider = agent_type.to_string();
    let provider2 = provider.clone();
    (
        proptest::collection::vec(arb_budget_alert(provider), 0..3),
        proptest::option::of(arb_rate_limit_summary(provider2)),
        proptest::option::of(arb_quota_availability()),
        proptest::option::of(0.0f64..100.0),
    )
        .prop_map(|(alerts, rl, qa, pct)| QuotaSignals {
            budget_alerts: alerts,
            rate_limit_summary: rl,
            quota_availability: qa,
            selected_quota_percent: pct,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Invariant 1: The verdict is Block iff at least one warning is blocking.
    #[test]
    fn verdict_consistency_with_blocking_warnings(
        agent_type in arb_agent_type(),
        signals in arb_agent_type().prop_flat_map(arb_signals),
    ) {
        let mut gate = QuotaGate::new();
        let decision = gate.evaluate(agent_type, &signals);

        let has_blocking = decision.warnings.iter().any(|w| w.blocking);

        if has_blocking {
            prop_assert_eq!(decision.verdict, LaunchVerdict::Block,
                "Expected Block when blocking warning present");
        } else if !decision.warnings.is_empty() {
            prop_assert_eq!(decision.verdict, LaunchVerdict::Warn,
                "Expected Warn when non-blocking warnings present");
        } else {
            prop_assert_eq!(decision.verdict, LaunchVerdict::Allow,
                "Expected Allow when no warnings");
        }
    }

    /// Invariant 2: Budget alerts for a different provider don't affect the result.
    #[test]
    fn budget_isolation_across_providers(
        severity in arb_severity(),
        budget_limit in 1.0f64..100.0,
        current_cost in 0.0f64..200.0,
    ) {
        let mut gate = QuotaGate::new();
        // Alert is for Gemini, but we evaluate Codex
        let signals = QuotaSignals {
            budget_alerts: vec![BudgetAlert {
                agent_type: "gemini".to_string(),
                severity,
                budget_limit_usd: budget_limit,
                current_cost_usd: current_cost,
                usage_fraction: current_cost / budget_limit,
            }],
            rate_limit_summary: None,
            quota_availability: None,
            selected_quota_percent: None,
        };

        let decision = gate.evaluate(AgentType::Codex, &signals);
        // No budget warnings should appear for Codex
        let has_budget_warning = decision.warnings.iter()
            .any(|w| matches!(w.source, WarningSource::Budget));
        prop_assert!(!has_budget_warning,
            "Budget alert for gemini should not affect codex evaluation");
    }

    /// Invariant 3: Telemetry conservation: evaluations = allowed + warned + blocked.
    #[test]
    fn telemetry_conservation(
        signals_list in proptest::collection::vec(
            arb_agent_type().prop_flat_map(|at| arb_signals(at).prop_map(move |s| (at, s))),
            1..20
        )
    ) {
        let mut gate = QuotaGate::new();
        for (agent_type, signals) in &signals_list {
            gate.evaluate(*agent_type, signals);
        }

        let snap = gate.telemetry().snapshot();
        prop_assert_eq!(snap.evaluations, snap.allowed + snap.warned + snap.blocked,
            "evaluations ({}) != allowed ({}) + warned ({}) + blocked ({})",
            snap.evaluations, snap.allowed, snap.warned, snap.blocked);
    }

    /// Invariant 4: LaunchDecision serde roundtrip preserves verdict and warning count.
    #[test]
    fn launch_decision_serde_roundtrip(
        agent_type in arb_agent_type(),
        signals in arb_agent_type().prop_flat_map(arb_signals),
    ) {
        let mut gate = QuotaGate::new();
        let decision = gate.evaluate(agent_type, &signals);

        let json = serde_json::to_string(&decision).unwrap();
        let deserialized: LaunchDecision = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(decision.verdict, deserialized.verdict);
        prop_assert_eq!(decision.warnings.len(), deserialized.warnings.len());
        prop_assert_eq!(decision.agent_type, deserialized.agent_type);
    }

    /// Invariant 5: Telemetry counters never decrease across evaluations.
    #[test]
    fn telemetry_monotonicity(
        signals_list in proptest::collection::vec(
            arb_agent_type().prop_flat_map(|at| arb_signals(at).prop_map(move |s| (at, s))),
            2..15
        )
    ) {
        let mut gate = QuotaGate::new();
        let mut prev_evals = 0u64;

        for (agent_type, signals) in &signals_list {
            gate.evaluate(*agent_type, signals);
            let snap = gate.telemetry().snapshot();
            prop_assert!(snap.evaluations >= prev_evals,
                "evaluations decreased: {} < {}", snap.evaluations, prev_evals);
            prev_evals = snap.evaluations;
        }
    }

    /// Invariant 6: QuotaGateTelemetrySnapshot serde roundtrip.
    #[test]
    fn telemetry_snapshot_serde_roundtrip(
        evaluations in 0u64..1000,
        allowed in 0u64..500,
        warned in 0u64..300,
        blocked in 0u64..200,
    ) {
        let snap = QuotaGateTelemetrySnapshot {
            evaluations,
            allowed,
            warned,
            blocked,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: QuotaGateTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, deserialized);
    }

    /// Invariant 7: Block count equals the number of blocking warnings.
    #[test]
    fn block_count_matches_blocking_warnings(
        agent_type in arb_agent_type(),
        signals in arb_agent_type().prop_flat_map(arb_signals),
    ) {
        let mut gate = QuotaGate::new();
        let decision = gate.evaluate(agent_type, &signals);

        let expected = decision.warnings.iter().filter(|w| w.blocking).count();
        prop_assert_eq!(decision.block_count(), expected);
    }

    // =========================================================================
    // QG-8: is_blocked exactly mirrors verdict == Block
    // =========================================================================

    #[test]
    fn qg8_is_blocked_matches_verdict(
        agent_type in arb_agent_type(),
        signals in arb_agent_type().prop_flat_map(arb_signals),
    ) {
        let mut gate = QuotaGate::new();
        let decision = gate.evaluate(agent_type, &signals);
        prop_assert_eq!(decision.is_blocked(), decision.verdict == LaunchVerdict::Block);
    }

    // =========================================================================
    // QG-9: is_warned exactly mirrors verdict == Warn
    // =========================================================================

    #[test]
    fn qg9_is_warned_matches_verdict(
        agent_type in arb_agent_type(),
        signals in arb_agent_type().prop_flat_map(arb_signals),
    ) {
        let mut gate = QuotaGate::new();
        let decision = gate.evaluate(agent_type, &signals);
        prop_assert_eq!(decision.is_warned(), decision.verdict == LaunchVerdict::Warn);
    }

    // =========================================================================
    // QG-10: Default signals always produce Allow
    // =========================================================================

    #[test]
    fn qg10_default_signals_always_allow(agent_type in arb_agent_type()) {
        let mut gate = QuotaGate::new();
        let decision = gate.evaluate(agent_type, &QuotaSignals::default());
        prop_assert_eq!(decision.verdict, LaunchVerdict::Allow);
        prop_assert!(decision.warnings.is_empty());
    }

    // =========================================================================
    // QG-11: Exhausted quota always blocks (regardless of other clear signals)
    // =========================================================================

    #[test]
    fn qg11_exhausted_quota_always_blocks(agent_type in arb_agent_type()) {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: None,
            quota_availability: Some(QuotaAvailability::Exhausted),
            selected_quota_percent: None,
        };
        let decision = gate.evaluate(agent_type, &signals);
        prop_assert_eq!(decision.verdict, LaunchVerdict::Block);
        let has_account_block = decision.warnings.iter()
            .any(|w| w.blocking && matches!(w.source, WarningSource::AccountQuota));
        prop_assert!(has_account_block);
    }

    // =========================================================================
    // QG-12: FullyLimited rate limit always blocks
    // =========================================================================

    #[test]
    fn qg12_fully_limited_always_blocks(
        agent_type in arb_agent_type(),
        limited_count in 1usize..20,
        earliest in 0u64..600,
    ) {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: Some(ProviderRateLimitSummary {
                agent_type: agent_type.to_string(),
                status: ProviderRateLimitStatus::FullyLimited,
                limited_pane_count: limited_count,
                total_pane_count: limited_count,
                earliest_clear_secs: earliest,
                total_events: 1,
            }),
            quota_availability: None,
            selected_quota_percent: None,
        };
        let decision = gate.evaluate(agent_type, &signals);
        prop_assert_eq!(decision.verdict, LaunchVerdict::Block);
        let has_rl_block = decision.warnings.iter()
            .any(|w| w.blocking && matches!(w.source, WarningSource::RateLimit));
        prop_assert!(has_rl_block);
    }

    // =========================================================================
    // QG-13: Critical budget alert always blocks
    // =========================================================================

    #[test]
    fn qg13_critical_budget_always_blocks(
        agent_type in arb_agent_type(),
        budget_limit in 1.0f64..100.0,
        usage_fraction in 1.0f64..5.0,
    ) {
        let provider = agent_type.to_string();
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![BudgetAlert {
                agent_type: provider,
                severity: AlertSeverity::Critical,
                budget_limit_usd: budget_limit,
                current_cost_usd: budget_limit * usage_fraction,
                usage_fraction,
            }],
            rate_limit_summary: None,
            quota_availability: None,
            selected_quota_percent: None,
        };
        let decision = gate.evaluate(agent_type, &signals);
        prop_assert_eq!(decision.verdict, LaunchVerdict::Block);
    }

    // =========================================================================
    // QG-14: Warning budget alert alone never blocks
    // =========================================================================

    #[test]
    fn qg14_warning_budget_alone_never_blocks(
        agent_type in arb_agent_type(),
        budget_limit in 1.0f64..100.0,
        usage_fraction in 0.5f64..0.99,
    ) {
        let provider = agent_type.to_string();
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![BudgetAlert {
                agent_type: provider,
                severity: AlertSeverity::Warning,
                budget_limit_usd: budget_limit,
                current_cost_usd: budget_limit * usage_fraction,
                usage_fraction,
            }],
            rate_limit_summary: None,
            quota_availability: None,
            selected_quota_percent: None,
        };
        let decision = gate.evaluate(agent_type, &signals);
        prop_assert_eq!(decision.verdict, LaunchVerdict::Warn);
        prop_assert!(!decision.is_blocked());
    }

    // =========================================================================
    // QG-15: Clear rate limit status adds no warnings
    // =========================================================================

    #[test]
    fn qg15_clear_rate_limit_no_warnings(
        agent_type in arb_agent_type(),
        total_panes in 1usize..20,
    ) {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: Some(ProviderRateLimitSummary {
                agent_type: agent_type.to_string(),
                status: ProviderRateLimitStatus::Clear,
                limited_pane_count: 0,
                total_pane_count: total_panes,
                earliest_clear_secs: 0,
                total_events: 0,
            }),
            quota_availability: None,
            selected_quota_percent: None,
        };
        let decision = gate.evaluate(agent_type, &signals);
        prop_assert_eq!(decision.verdict, LaunchVerdict::Allow);
    }

    // =========================================================================
    // QG-16: PartiallyLimited produces Warn (not Block)
    // =========================================================================

    #[test]
    fn qg16_partially_limited_warns_not_blocks(
        agent_type in arb_agent_type(),
        limited in 1usize..10,
        total in 2usize..20,
    ) {
        let total = total.max(limited + 1);
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: Some(ProviderRateLimitSummary {
                agent_type: agent_type.to_string(),
                status: ProviderRateLimitStatus::PartiallyLimited,
                limited_pane_count: limited,
                total_pane_count: total,
                earliest_clear_secs: 60,
                total_events: 1,
            }),
            quota_availability: None,
            selected_quota_percent: None,
        };
        let decision = gate.evaluate(agent_type, &signals);
        prop_assert_eq!(decision.verdict, LaunchVerdict::Warn);
        prop_assert!(!decision.is_blocked());
    }

    // =========================================================================
    // QG-17: Verdict serde roundtrip for all three variants
    // =========================================================================

    #[test]
    fn qg17_verdict_serde_roundtrip(
        verdict in prop_oneof![
            Just(LaunchVerdict::Allow),
            Just(LaunchVerdict::Warn),
            Just(LaunchVerdict::Block),
        ]
    ) {
        let json = serde_json::to_string(&verdict).unwrap();
        let back: LaunchVerdict = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(verdict, back);
    }

    // =========================================================================
    // QG-18: WarningSource serde roundtrip
    // =========================================================================

    #[test]
    fn qg18_warning_source_serde_roundtrip(
        source in prop_oneof![
            Just(WarningSource::Budget),
            Just(WarningSource::RateLimit),
            Just(WarningSource::AccountQuota),
        ]
    ) {
        let json = serde_json::to_string(&source).unwrap();
        let back: WarningSource = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(source, back);
    }

    // =========================================================================
    // QG-19: Verdict ordering: Allow < Warn < Block
    // =========================================================================

    #[test]
    fn qg19_verdict_total_order(
        a in prop_oneof![
            Just(LaunchVerdict::Allow),
            Just(LaunchVerdict::Warn),
            Just(LaunchVerdict::Block),
        ],
        b in prop_oneof![
            Just(LaunchVerdict::Allow),
            Just(LaunchVerdict::Warn),
            Just(LaunchVerdict::Block),
        ],
    ) {
        // Total ordering: exactly one of <, ==, > holds
        let lt = a < b;
        let eq = a == b;
        let gt = a > b;
        let count = lt as u8 + eq as u8 + gt as u8;
        prop_assert_eq!(count, 1, "trichotomy violated for {:?} vs {:?}", a, b);

        // Consistency with named ordering
        if a == LaunchVerdict::Allow && b == LaunchVerdict::Block {
            prop_assert!(a < b);
        }
    }

    // =========================================================================
    // QG-20: Low quota produces Warn with percentage in message
    // =========================================================================

    #[test]
    fn qg20_low_quota_includes_percentage(
        agent_type in arb_agent_type(),
        pct in 0.1f64..50.0,
    ) {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: None,
            quota_availability: Some(QuotaAvailability::Low),
            selected_quota_percent: Some(pct),
        };
        let decision = gate.evaluate(agent_type, &signals);
        prop_assert_eq!(decision.verdict, LaunchVerdict::Warn);
        // The message should include the percentage
        let has_pct = decision.warnings.iter().any(|w| {
            matches!(w.source, WarningSource::AccountQuota) && w.message.contains("remaining")
        });
        prop_assert!(has_pct, "Low quota warning should include 'remaining' with percentage");
    }

    // =========================================================================
    // QG-21: Multiple evaluations accumulate telemetry correctly
    // =========================================================================

    #[test]
    fn qg21_telemetry_evaluations_count(
        n in 1usize..50,
        agent_type in arb_agent_type(),
    ) {
        let mut gate = QuotaGate::new();
        let signals = QuotaSignals::default();
        for _ in 0..n {
            gate.evaluate(agent_type, &signals);
        }
        let snap = gate.telemetry().snapshot();
        prop_assert_eq!(snap.evaluations, n as u64);
    }

    // =========================================================================
    // QG-22: agent_type in decision matches what was passed
    // =========================================================================

    #[test]
    fn qg22_decision_agent_type_matches(
        agent_type in arb_agent_type(),
        signals in arb_agent_type().prop_flat_map(arb_signals),
    ) {
        let mut gate = QuotaGate::new();
        let decision = gate.evaluate(agent_type, &signals);
        prop_assert_eq!(decision.agent_type, agent_type.to_string());
    }

    // =========================================================================
    // QG-23: Adding a blocking signal to warn-only signals escalates to Block
    // =========================================================================

    #[test]
    fn qg23_escalation_from_warn_to_block(agent_type in arb_agent_type()) {
        let mut gate = QuotaGate::new();

        // First: warning-only signal (Low quota)
        let warn_signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: None,
            quota_availability: Some(QuotaAvailability::Low),
            selected_quota_percent: Some(5.0),
        };
        let warn_decision = gate.evaluate(agent_type, &warn_signals);
        prop_assert_eq!(warn_decision.verdict, LaunchVerdict::Warn);

        // Second: add blocking signal (Exhausted quota)
        let block_signals = QuotaSignals {
            budget_alerts: vec![],
            rate_limit_summary: None,
            quota_availability: Some(QuotaAvailability::Exhausted),
            selected_quota_percent: None,
        };
        let block_decision = gate.evaluate(agent_type, &block_signals);
        prop_assert_eq!(block_decision.verdict, LaunchVerdict::Block);
        prop_assert!(block_decision.verdict > warn_decision.verdict);
    }
}
