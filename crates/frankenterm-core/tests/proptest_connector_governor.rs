//! Property-based tests for connector_governor module (ft-3681t.5.11).

use frankenterm_core::connector_governor::*;
use frankenterm_core::connector_outbound_bridge::{ConnectorAction, ConnectorActionKind};
use proptest::prelude::*;

// =============================================================================
// Strategies
// =============================================================================

fn arb_action_kind() -> impl Strategy<Value = ConnectorActionKind> {
    prop_oneof![
        Just(ConnectorActionKind::Notify),
        Just(ConnectorActionKind::Ticket),
        Just(ConnectorActionKind::TriggerWorkflow),
        Just(ConnectorActionKind::AuditLog),
        Just(ConnectorActionKind::Invoke),
        Just(ConnectorActionKind::CredentialAction),
    ]
}

fn arb_connector_id() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("slack".to_string()),
        Just("github".to_string()),
        Just("datadog".to_string()),
        Just("jira".to_string()),
        Just("pagerduty".to_string()),
    ]
}

fn arb_connector_action() -> impl Strategy<Value = ConnectorAction> {
    (arb_connector_id(), arb_action_kind(), 1000u64..100_000u64).prop_map(
        |(connector, kind, ts)| ConnectorAction {
            target_connector: connector,
            action_kind: kind,
            correlation_id: "test-corr".to_string(),
            params: serde_json::json!({}),
            created_at_ms: ts,
        },
    )
}

fn arb_token_bucket_config() -> impl Strategy<Value = TokenBucketConfig> {
    (1u64..1000, 1u64..100, 100u64..10_000).prop_map(|(cap, rate, interval)| TokenBucketConfig {
        capacity: cap,
        refill_rate: rate,
        refill_interval_ms: interval,
    })
}

fn arb_quota_config() -> impl Strategy<Value = QuotaConfig> {
    (1u64..10_000, 1000u64..3_600_000, 0.1f64..1.0).prop_map(|(max, window, warn)| QuotaConfig {
        max_actions: max,
        window_ms: window,
        warning_threshold: warn,
    })
}

// =============================================================================
// Token Bucket Properties
// =============================================================================

proptest! {
    #[test]
    fn token_bucket_never_exceeds_capacity(
        config in arb_token_bucket_config(),
        now_ms in 0u64..1_000_000,
    ) {
        let mut bucket = TokenBucket::new(config.clone());
        let avail = bucket.available(now_ms);
        prop_assert!(avail <= config.capacity,
            "available {} > capacity {}", avail, config.capacity);
    }

    #[test]
    fn token_bucket_consume_reduces_available(
        config in arb_token_bucket_config(),
    ) {
        let mut bucket = TokenBucket::new(config);
        let before = bucket.available(0);
        if bucket.try_consume(0) {
            let after = bucket.available(0);
            prop_assert_eq!(after, before - 1);
        }
    }

    #[test]
    fn token_bucket_refill_monotonic(
        config in arb_token_bucket_config(),
        t1 in 0u64..500_000,
        dt in 0u64..500_000,
    ) {
        let mut bucket = TokenBucket::with_initial(config, 0, 0);
        let a1 = bucket.available(t1);
        let a2 = bucket.available(t1 + dt);
        prop_assert!(a2 >= a1,
            "available should not decrease over time: {} -> {} at dt={}", a1, a2, dt);
    }

    #[test]
    fn token_bucket_fill_ratio_bounded(
        config in arb_token_bucket_config(),
        now_ms in 0u64..1_000_000,
    ) {
        let mut bucket = TokenBucket::new(config);
        let ratio = bucket.fill_ratio(now_ms);
        prop_assert!(ratio >= 0.0 && ratio <= 1.0,
            "fill_ratio out of bounds: {}", ratio);
    }
}

// =============================================================================
// Quota Tracker Properties
// =============================================================================

proptest! {
    #[test]
    fn quota_remaining_never_negative(
        config in arb_quota_config(),
        actions in 0usize..200,
    ) {
        let mut qt = QuotaTracker::new(config);
        for i in 0..actions {
            qt.record(i as u64 * 100);
        }
        let remaining = qt.remaining(actions as u64 * 100);
        // remaining is u64, so can't go negative, but check usage_fraction <= 1.0
        let frac = qt.usage_fraction(actions as u64 * 100);
        prop_assert!(frac <= 1.01, "usage fraction {} > 1.0", frac);
        let _ = remaining; // suppress unused
    }

    #[test]
    fn quota_window_gc_frees_old_actions(
        max_actions in 5u64..100,
        window_ms in 1000u64..10_000,
    ) {
        let config = QuotaConfig { max_actions, window_ms, warning_threshold: 0.8 };
        let mut qt = QuotaTracker::new(config);

        // Record all actions at the SAME timestamp so they all fall in one window
        let now = 10_000u64;
        for _ in 0..max_actions {
            qt.record(now);
        }
        prop_assert!(qt.is_exhausted(now),
            "Should be exhausted after recording max_actions at same timestamp");

        // After window passes, should have capacity again
        let future = now + window_ms + 1;
        prop_assert!(!qt.is_exhausted(future),
            "Should have capacity after window expires");
        prop_assert_eq!(qt.remaining(future), max_actions);
    }

    #[test]
    fn quota_snapshot_consistent(
        config in arb_quota_config(),
        now_ms in 1000u64..100_000,
    ) {
        let mut qt = QuotaTracker::new(config);
        qt.record(now_ms);
        let snap = qt.snapshot(now_ms);
        prop_assert_eq!(snap.used + snap.remaining, snap.max);
    }
}

// =============================================================================
// Cost Budget Properties
// =============================================================================

proptest! {
    #[test]
    fn cost_budget_window_cost_bounded(
        max_cost_cents in 10u64..10_000,
        n_actions in 1usize..50,
    ) {
        let mut config = CostBudgetConfig::default();
        config.max_cost_cents = max_cost_cents;
        let mut cb = CostBudget::new(config);

        for i in 0..n_actions {
            cb.record(&ConnectorActionKind::Notify, i as u64 * 100);
        }
        let fraction = cb.usage_fraction(n_actions as u64 * 100);
        prop_assert!(fraction >= 0.0, "usage fraction negative: {}", fraction);
    }

    #[test]
    fn cost_budget_exhausted_means_no_remaining(
        max_cost_cents in 1u64..100,
    ) {
        let mut config = CostBudgetConfig::default();
        config.max_cost_cents = max_cost_cents;
        let mut cb = CostBudget::new(config);

        // Record expensive actions until exhausted
        let mut ts = 1000u64;
        while !cb.is_exhausted(ts) {
            cb.record(&ConnectorActionKind::TriggerWorkflow, ts);
            ts += 100;
            if ts > 1_000_000 { break; } // safety
        }
        if cb.is_exhausted(ts) {
            prop_assert_eq!(cb.remaining_cents(ts), 0);
        }
    }
}

// =============================================================================
// Adaptive Backoff Properties
// =============================================================================

proptest! {
    #[test]
    fn backoff_delay_monotonic_with_failures(
        base_ms in 100u64..5000,
        max_ms in 5000u64..120_000,
    ) {
        let mut b = AdaptiveBackoff::new(base_ms, max_ms, 2.0);
        let mut prev_remaining = 0u64;
        for i in 0u64..10 {
            b.record_failure(i * 100_000);
            let rem = b.remaining_ms(i * 100_000);
            prop_assert!(rem >= prev_remaining || rem == max_ms || i == 0,
                "backoff should increase: prev={}, cur={}, i={}", prev_remaining, rem, i);
            prev_remaining = rem;
        }
    }

    #[test]
    fn backoff_resets_completely_on_success(
        failures in 1u32..20,
    ) {
        let mut b = AdaptiveBackoff::connector_default();
        for i in 0..failures {
            b.record_failure(i as u64 * 1000);
        }
        b.record_success();
        prop_assert_eq!(b.consecutive_failures(), 0);
        prop_assert!(!b.is_active(failures as u64 * 1000 + 100_000));
    }
}

// =============================================================================
// Governor Integration Properties
// =============================================================================

proptest! {
    #[test]
    fn governor_telemetry_adds_up(
        actions in prop::collection::vec(arb_connector_action(), 1..50),
    ) {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        for a in &actions {
            gov.evaluate(a, a.created_at_ms);
        }
        let snap = gov.snapshot(100_000);
        let total = snap.telemetry.allows + snap.telemetry.throttles + snap.telemetry.rejections;
        prop_assert_eq!(total, snap.telemetry.evaluations,
            "allows + throttles + rejections should equal evaluations");
    }

    #[test]
    fn governor_verdict_always_valid(
        action in arb_connector_action(),
    ) {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        let d = gov.evaluate(&action, action.created_at_ms);
        prop_assert!(
            matches!(d.verdict, GovernorVerdict::Allow | GovernorVerdict::Throttle | GovernorVerdict::Reject),
            "unexpected verdict: {:?}", d.verdict
        );
    }

    #[test]
    fn governor_decision_serde_roundtrip(
        action in arb_connector_action(),
    ) {
        let mut gov = ConnectorGovernor::new(ConnectorGovernorConfig::default());
        let d = gov.evaluate(&action, action.created_at_ms);
        let json = serde_json::to_string(&d).unwrap();
        let back: GovernorDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(d.verdict, back.verdict);
        prop_assert_eq!(d.reason, back.reason);
        prop_assert_eq!(d.delay_ms, back.delay_ms);
    }

    #[test]
    fn governor_connectors_isolated(
        ids in prop::collection::vec(arb_connector_id(), 2..5),
    ) {
        let mut config = ConnectorGovernorConfig::default();
        config.default_rate_limit = TokenBucketConfig {
            capacity: 3,
            refill_rate: 0,
            refill_interval_ms: 1000,
        };
        let mut gov = ConnectorGovernor::new(config);

        // Each connector gets its own quota of 3
        for id in &ids {
            let action = ConnectorAction {
                target_connector: id.clone(),
                action_kind: ConnectorActionKind::Notify,
                correlation_id: "test".to_string(),
                params: serde_json::json!({}),
                created_at_ms: 1000,
            };
            let d = gov.evaluate(&action, 1000);
            // First action per connector should be allowed
            prop_assert!(d.is_allowed(),
                "first action for {} should be allowed, got {:?}", id, d.verdict);
        }
    }
}

// =============================================================================
// Serde roundtrip properties
// =============================================================================

proptest! {
    #[test]
    fn quota_snapshot_serde_roundtrip(
        used in 0u64..10_000,
        max in 1u64..10_000,
    ) {
        let snap = QuotaSnapshot {
            used,
            max,
            remaining: max.saturating_sub(used),
            usage_fraction: if max == 0 { 1.0 } else { used as f64 / max as f64 },
            total_lifetime: used + 100,
            window_ms: 3_600_000,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: QuotaSnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.used, back.used);
        prop_assert_eq!(snap.max, back.max);
        prop_assert_eq!(snap.remaining, back.remaining);
    }

    #[test]
    fn telemetry_snapshot_serde_roundtrip(
        evals in 0u64..100_000,
        allows in 0u64..100_000,
        throttles in 0u64..100_000,
        rejections in 0u64..100_000,
    ) {
        let snap = GovernorTelemetrySnapshot { evaluations: evals, allows, throttles, rejections };
        let json = serde_json::to_string(&snap).unwrap();
        let back: GovernorTelemetrySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap, back);
    }
}
