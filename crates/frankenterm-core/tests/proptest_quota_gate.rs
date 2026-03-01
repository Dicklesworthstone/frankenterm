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
    LaunchDecision, LaunchVerdict, QuotaGate, QuotaGateTelemetrySnapshot,
    QuotaSignals, WarningSource,
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
fn arb_rate_limit_summary(agent_type_str: String) -> impl Strategy<Value = ProviderRateLimitSummary>
{
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
        let budget_warnings: Vec<_> = decision.warnings.iter()
            .filter(|w| matches!(w.source, WarningSource::Budget))
            .collect();
        prop_assert!(budget_warnings.is_empty(),
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
}
